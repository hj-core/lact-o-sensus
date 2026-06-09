//! # Lact-O-Sensus Node Server
//!
//! This crate serves as the **Composition Root** for the Lact-O-Sensus
//! distributed node. It implements the "Tri-Layer Onion" architecture (ADR 009)
//! by integrating the Consensus Engine, Persistent State Machine (FSM), and
//! gRPC Delivery Layer.
//!
//! The initialization sequence follows a strict lifecycle:
//! 1. **Physical Foundation**: Telemetry, Configuration, and Persistent Storage
//!    (sled).
//! 2. **Logical Orchestrator**: Identity verification and Cold-Boot Recovery.
//! 3. **Execution Shell**: gRPC service dispatching and deterministic tick
//!    loop.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use common::proto::v1::app::ingress_service_server::IngressServiceServer;
use common::proto::v1::raft::consensus_service_server::ConsensusServiceServer;
use common::types::NodeId;
use common::types::identity::NodeIdentity;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use common_rpc::IdentityInterceptor;
use common_rpc::TraceInterceptor;
use gateway::ingress::IngressConfig;
use gateway::ingress::IngressDispatcher;
use gateway::veto::GrpcVetoRelay;
use lacto_fsm::LactoStore;
use raft_engine::config::Config;
use raft_engine::consensus::spawn_background_applier;
use raft_engine::consensus::spawn_tick_loop;
use raft_engine::engine::LogicalNode;
use raft_engine::identity::initialize_node_identity;
use raft_engine::peer::PeerManager;
use raft_engine::recovery::RecoveryManager;
use raft_engine::service::consensus::ConsensusDispatcher;
use raft_engine::service::handle::LocalRaftHandle;
use raft_engine::shell::ConsensusShell;
use raft_engine::storage::SledStorage;
use rand::RngExt;
use rand::SeedableRng;
use sled::Db;
use tonic::service::Interceptor;
use tonic::transport::Server;
use tracing::Instrument;
use tracing::Span;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

/// The main entry point for the Lact-O-Sensus node.
///
/// Orchestrates the "Tri-Layer Onion" initialization sequence (ADR 009) and
/// establishes the transport layer.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    setup_telemetry();

    info!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        "Lact-O-Sensus Node Initializing..."
    );

    let config = Arc::new(load_configuration(&args.config)?);

    // 1. Physical Foundation: Isolated Persistence (sled) (ADR 001/009)
    let (system_db, log_db, fsm_db) = init_persistence(&config)?;

    // 2. Logical Orchestrator: Identity & Recovery (ADR 004/009)
    let identity = Arc::new(init_identity(&system_db, &config)?);

    let root_span = info_span!(
        "node",
        cluster_id = %identity.cluster_id(),
        node_id = %identity.node_id()
    );

    let (fsm, shared_state) =
        init_node_state(&identity, &config, &log_db, &fsm_db, &root_span).await?;

    // 3. Execution Shell: Networking & Service Dispatchers (Phase 4)
    let (peer_manager, consensus_dispatcher, ingress_dispatcher) =
        init_dispatchers(&identity, &config, &shared_state, &fsm, &root_span)?;

    run_server(
        config,
        identity,
        shared_state,
        peer_manager,
        consensus_dispatcher,
        ingress_dispatcher,
        system_db,
        log_db,
        fsm_db,
        root_span,
    )
    .await
}

/// Initializes the `tracing` ecosystem with clinical defaults.
fn setup_telemetry() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}

/// Loads the node configuration from the specified path.
#[instrument(target = "clinical::foundation", skip(path), fields(path = %path.display()))]
fn load_configuration(path: &PathBuf) -> Result<Config> {
    Config::load(path).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to load configuration"
        );
        e.into()
    })
}

/// Initializes isolated persistence databases for the system, log, and FSM.
#[instrument(target = "clinical::foundation", skip(config))]
fn init_persistence(config: &Config) -> Result<(Db, Db, Db)> {
    let system_path = config.data_dir.join("system");
    let log_path = config.data_dir.join("log");
    let fsm_path = config.data_dir.join("fsm");

    info!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        path = %system_path.display(),
        "Opening system database"
    );
    let system_db = sled::open(&system_path).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to open system database"
        );
        anyhow::Error::from(e)
    })?;

    info!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        path = %log_path.display(),
        "Opening log database"
    );
    let log_db = sled::open(&log_path).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to open log database"
        );
        anyhow::Error::from(e)
    })?;

    info!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        path = %fsm_path.display(),
        "Opening FSM database"
    );
    let fsm_db = sled::open(&fsm_path).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to open FSM database"
        );
        anyhow::Error::from(e)
    })?;

    Ok((system_db, log_db, fsm_db))
}

/// Verifies or initializes the node's cluster identity (ADR 004).
#[instrument(target = "clinical::foundation", skip(system_db, config))]
fn init_identity(system_db: &Db, config: &Config) -> Result<NodeIdentity> {
    initialize_node_identity(system_db, config).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Fatal Error during identity verification"
        );
        e.into()
    })
}

/// Initializes the shared node state, storage, and performs cold-boot recovery.
#[instrument(
    target = "clinical::foundation",
    skip(identity, config, log_db, fsm_db, root_span)
)]
async fn init_node_state(
    identity: &Arc<NodeIdentity>,
    config: &Config,
    log_db: &Db,
    fsm_db: &Db,
    root_span: &Span,
) -> Result<(Arc<LactoStore>, Arc<ConsensusShell<LactoStore>>)> {
    let fsm_store = LactoStore::new(fsm_db.clone()).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to initialize LactoStore"
        );
        anyhow::anyhow!("Failed to initialize LactoStore: {}", e)
    })?;
    let fsm = Arc::new(fsm_store);

    let storage = SledStorage::new(log_db.clone()).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to initialize SledStorage"
        );
        anyhow::anyhow!("Failed to initialize SledStorage: {}", e)
    })?;
    let storage = Arc::new(storage);

    // Cold-Boot Recovery (ADR 001/009)
    let trace_id = TraceId::generate();
    let recovery_span = info_span!(
        target: ClinicalTarget::ClinicalRecovery.as_str(),
        parent: root_span,
        "cold_boot_recovery",
        trace_id = %trace_id
    );

    let recovery = RecoveryManager::new(fsm.clone(), storage.clone());
    info!(
        target: ClinicalTarget::ClinicalRecovery.as_str(),
        %trace_id,
        "Commencing cold-boot recovery..."
    );
    recovery
        .recover()
        .instrument(recovery_span)
        .await
        .map_err(|e| {
            error!(
                target: ClinicalTarget::ClinicalRecovery.as_str(),
                error = %e,
                "Cold-boot recovery failed"
            );
            anyhow::anyhow!("Cold-boot recovery failed: {}", e)
        })?;

    let thresholds = config.raft.calculate_thresholds();
    let rng = create_deterministic_rng(identity.node_id());

    let logical_node = LogicalNode::try_new(
        identity.clone(),
        fsm.clone(),
        storage.clone(),
        thresholds,
        rng,
    )
    .map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Failed to initialize LogicalNode"
        );
        anyhow::anyhow!("Failed to initialize LogicalNode: {}", e)
    })?;

    let shared_state = Arc::new(ConsensusShell::new(logical_node));

    Ok((fsm, shared_state))
}

/// Initializes the peer manager and service dispatchers.
#[instrument(
    target = "clinical::foundation",
    skip(identity, config, shared_state, fsm, _root_span)
)]
fn init_dispatchers(
    identity: &Arc<NodeIdentity>,
    config: &Arc<Config>,
    shared_state: &Arc<ConsensusShell<LactoStore>>,
    fsm: &Arc<LactoStore>,
    _root_span: &Span,
) -> Result<(
    Arc<PeerManager>,
    ConsensusDispatcher<LactoStore>,
    IngressDispatcher,
)> {
    let peer_manager = PeerManager::try_new(identity.clone(), &config.peers).map_err(|e| {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %e,
            "Fatal Error during Peer Manager initialization"
        );
        anyhow::Error::from(e)
    })?;
    let peer_manager = Arc::new(peer_manager);

    let consensus_dispatcher = ConsensusDispatcher::new(identity.clone(), shared_state.clone());

    let raft_handle = Arc::new(LocalRaftHandle::new(
        config.clone(),
        shared_state.clone(),
        peer_manager.clone(),
    ));

    let veto_channel = config
        .policy
        .veto_endpoint()
        .map_err(|e| {
            error!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                error = %e,
                "Failed to parse AI Veto address"
            );
            anyhow::anyhow!("Failed to parse AI Veto address: {}", e)
        })?
        .connect_lazy();
    let veto_relay = Arc::new(GrpcVetoRelay::new(
        veto_channel,
        identity.cluster_id().clone(),
        identity.node_id(),
    ));

    let ingress_dispatcher = IngressDispatcher::new(
        raft_handle,
        fsm.clone(),
        fsm.clone(),
        veto_relay,
        IngressConfig {
            veto_timeout: config.policy.veto_timeout(),
            consensus_timeout: config.raft.consensus_timeout(),
            veto_max_retries: config.policy.veto_max_retries,
            max_justification_len: config.policy.max_justification_len,
        },
    );

    Ok((peer_manager, consensus_dispatcher, ingress_dispatcher))
}

/// Starts the gRPC server and manages the node's graceful shutdown.
#[allow(clippy::too_many_arguments)]
async fn run_server(
    config: Arc<Config>,
    identity: Arc<NodeIdentity>,
    shared_state: Arc<ConsensusShell<LactoStore>>,
    peer_manager: Arc<PeerManager>,
    consensus_dispatcher: ConsensusDispatcher<LactoStore>,
    ingress_dispatcher: IngressDispatcher,
    system_db: Db,
    log_db: Db,
    fsm_db: Db,
    root_span: Span,
) -> Result<()> {
    async move {
        // Spawn the unified deterministic Tick Loop
        spawn_tick_loop(config.clone(), shared_state.clone(), peer_manager.clone());

        // Spawn the continuous background FSM applier
        spawn_background_applier(shared_state.clone());

        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "Identity verified. Transport layer starting..."
        );

        let addr = config.listen_addr;
        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            address = %addr,
            "Starting gRPC server"
        );

        let shutdown = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    error = %e,
                    "Failed to install CTRL+C handler"
                );
            } else {
                info!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    "Shutdown signal received. Commencing graceful exit..."
                );
            }
        };

        let identity_interceptor = IdentityInterceptor::new(identity.clone());
        let ingress_trace_interceptor = TraceInterceptor::authoritative();
        let consensus_trace_interceptor = TraceInterceptor::propagative();

        Server::builder()
            .add_service(ConsensusServiceServer::with_interceptor(
                consensus_dispatcher,
                {
                    let mut identity = identity_interceptor.clone();
                    let mut trace = consensus_trace_interceptor;
                    move |req| identity.call(req).and_then(|req| trace.call(req))
                },
            ))
            .add_service(IngressServiceServer::with_interceptor(
                ingress_dispatcher,
                {
                    let mut identity = identity_interceptor;
                    let mut trace = ingress_trace_interceptor;
                    move |req| identity.call(req).and_then(|req| trace.call(req))
                },
            ))
            .serve_with_shutdown(addr, shutdown)
            .await
            .map_err(|e| {
                error!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    error = %e,
                    "gRPC server failed"
                );
                anyhow::Error::from(e)
            })?;

        // Persistence Cleanup (ADR 001: Sync-before-ACK / Crash-Recovery)
        let shutdown_span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "persistence_shutdown"
        );
        async {
            info!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                "gRPC server stopped. Flushing databases to disk..."
            );
            system_db.flush_async().await.map_err(anyhow::Error::from)?;
            log_db.flush_async().await.map_err(anyhow::Error::from)?;
            fsm_db.flush_async().await.map_err(anyhow::Error::from)?;
            info!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                "Databases synchronized successfully."
            );
            Ok::<(), anyhow::Error>(())
        }
        .instrument(shutdown_span)
        .await?;

        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "Node lifecycle finished successfully. Goodbye."
        );
        Ok(())
    }
    .instrument(root_span)
    .await
}

/// Creates a deterministically seeded PRNG for the Raft engine.
///
/// Combines the NodeId (to ensure cluster-wide uniqueness) with OS-level
/// entropy (to ensure cross-restart uniqueness) to prevent split-vote storms
/// (ADR 003).
fn create_deterministic_rng(node_id: NodeId) -> rand::rngs::StdRng {
    let seed = generate_deterministic_seed(node_id);
    rand::rngs::StdRng::from_seed(seed)
}

/// Generates a 256-bit seed derived from NodeId and OS entropy.
fn generate_deterministic_seed(node_id: NodeId) -> [u8; 32] {
    let mut seed = [0u8; 32];
    // Mix in the NodeId to guarantee uniqueness between nodes
    let node_id_bytes = node_id.as_u64().to_be_bytes();
    seed[0..8].copy_from_slice(&node_id_bytes);

    // Mix in OS entropy for cross-restart uniqueness
    rand::rng().fill(&mut seed[8..]);
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    mod generate_deterministic_seed {
        use super::*;

        mod behavior {
            use super::*;

            #[test]
            fn should_mix_in_node_id_in_first_8_bytes() {
                let node_id_val = 0xDEADBEEF_u64;
                let node_id = NodeId::try_new(node_id_val).unwrap();
                let seed = generate_deterministic_seed(node_id);

                let expected_bytes = node_id_val.to_be_bytes();
                assert_eq!(seed[0..8], expected_bytes);
            }

            #[test]
            fn should_populate_remaining_bytes_with_entropy() {
                let node_id = NodeId::try_new(1).unwrap();
                let seed_1 = generate_deterministic_seed(node_id);
                let seed_2 = generate_deterministic_seed(node_id);

                // OS entropy ensures these are different even for the same NodeId
                assert_ne!(seed_1[8..], seed_2[8..]);
            }
        }
    }
}
