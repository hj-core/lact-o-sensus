use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use common::proto::v1::app::ingress_service_server::IngressServiceServer;
use common::proto::v1::raft::consensus_service_server::ConsensusServiceServer;
use common::rpc::IdentityInterceptor;
use common::rpc::TraceInterceptor;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use gateway::ingress::IngressDispatcher;
use gateway::veto::GrpcVetoRelay;
use lacto_fsm::LactoStore;
use raft_engine::config::Config;
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
use tonic::service::Interceptor;
use tonic::transport::Server;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI Arguments
    let args = Args::parse();

    // 2. Initialize logging with EnvFilter (default to INFO)
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    info!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        "Lact-O-Sensus Node Initializing..."
    );

    // 3. Load Configuration
    let config = {
        let _span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "configuration_loading"
        )
        .entered();

        match Config::load(&args.config) {
            Ok(cfg) => Arc::new(cfg),
            Err(e) => {
                error!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    error = %e,
                    "Failed to load configuration from {:?}",
                    args.config
                );
                return Err(e.into());
            }
        }
    };

    // 4. Initialize Isolated Persistence (sled) (ADR 001/009)
    // Establishing split databases for strict component isolation.
    let (system_db, log_db, fsm_db) = {
        let _span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "persistence_initialization"
        )
        .entered();

        let system_path = config.data_dir.join("system");
        let log_path = config.data_dir.join("log");
        let fsm_path = config.data_dir.join("fsm");

        info!(path = %system_path.display(), "Opening system database");
        let system_db = sled::open(&system_path).map_err(anyhow::Error::from)?;

        info!(path = %log_path.display(), "Opening log database");
        let log_db = sled::open(&log_path).map_err(anyhow::Error::from)?;

        info!(path = %fsm_path.display(), "Opening FSM database");
        let fsm_db = sled::open(&fsm_path).map_err(anyhow::Error::from)?;

        (system_db, log_db, fsm_db)
    };

    // 5. Verify or Initialize Identity (ADR 004)
    let identity = {
        let _span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "identity_initialization"
        )
        .entered();

        match initialize_node_identity(&system_db, &config) {
            Ok(id) => Arc::new(id),
            Err(e) => {
                error!(error = %e, "Fatal Error during identity verification");
                return Err(e.into());
            }
        }
    };

    // 6. Create the Root Node Span (Established once identity is verified)
    let root_span = info_span!(
        "node",
        cluster_id = %identity.cluster_id(),
        node_id = %identity.node_id()
    );

    // 7. Initialize the Shared Node State & Recovery (Phase 3)
    let (fsm, shared_state) = {
        let _span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            parent: &root_span,
            "state_initialization"
        )
        .entered();

        let fsm_store = LactoStore::new(fsm_db.clone())
            .map_err(|e| anyhow::anyhow!("Failed to initialize LactoStore: {}", e))?;
        let fsm = Arc::new(fsm_store);
        let storage = Arc::new(
            SledStorage::new(log_db.clone())
                .map_err(|e| anyhow::anyhow!("Failed to initialize SledStorage: {}", e))?,
        );

        // 7.1 Cold-Boot Recovery (ADR 001/009)
        // Synchronize FSM with Consensus Log before accepting any network events.
        let trace_id = TraceId::generate();
        let recovery_span = info_span!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            parent: &root_span,
            "cold_boot_recovery",
            trace_id = %trace_id
        );
        let recovery = RecoveryManager::new(identity.clone(), fsm.clone(), storage.clone());
        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            %trace_id,
            "Commencing cold-boot recovery..."
        );
        recovery
            .recover()
            .instrument(recovery_span)
            .await
            .map_err(|e| anyhow::anyhow!("Cold-boot recovery failed: {}", e))?;

        let thresholds = config.raft.calculate_thresholds();

        let rng = create_deterministic_rng(identity.node_id());

        let logical_node = LogicalNode::try_new(
            identity.clone(),
            fsm.clone(),
            storage.clone(),
            thresholds,
            rng,
        )
        .map_err(|e| anyhow::anyhow!("Failed to initialize LogicalNode: {}", e))?;

        let shared_state = Arc::new(ConsensusShell::new(logical_node));

        (fsm, shared_state)
    };

    // 8. Initialize Networking & Service Dispatchers (Phase 4)
    let (peer_manager, consensus_dispatcher, ingress_dispatcher) = {
        let _span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            parent: &root_span,
            "service_initialization"
        )
        .entered();

        let peer_manager = Arc::new(match PeerManager::new(identity.clone(), &config.peers) {
            Ok(m) => m,
            Err(e) => {
                error!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    error = %e,
                    "Fatal Error during Peer Manager initialization"
                );
                return Err(e.into());
            }
        });

        let consensus_dispatcher = ConsensusDispatcher::new(identity.clone(), shared_state.clone());

        let raft_handle = Arc::new(LocalRaftHandle::new(
            shared_state.clone(),
            peer_manager.clone(),
        ));

        let veto_channel = config
            .policy
            .veto_endpoint()
            .map_err(|e| anyhow::anyhow!("Failed to parse AI Veto address: {}", e))?
            .connect_lazy();
        let veto_relay = Arc::new(GrpcVetoRelay::new(veto_channel));

        let ingress_dispatcher = IngressDispatcher::new(
            raft_handle,
            fsm.clone(),
            fsm.clone(),
            veto_relay,
            config.policy.veto_timeout(),
            config.policy.veto_max_retries,
            config.policy.max_justification_len,
        );

        (peer_manager, consensus_dispatcher, ingress_dispatcher)
    };

    let identity_interceptor = IdentityInterceptor::new(identity.clone());
    let ingress_trace_interceptor = TraceInterceptor::authoritative();
    let consensus_trace_interceptor = TraceInterceptor::propagative();

    async move {
        // 10. Spawn the unified deterministic Tick Loop
        // Spawning inside this block ensures it is a child task of the root node span.
        spawn_tick_loop(config.clone(), shared_state.clone(), peer_manager.clone());

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

        // Define the graceful shutdown signal
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

        // 11. Start the gRPC Server with combined interceptors
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
            .map_err(anyhow::Error::from)?;

        // 12. Persistence Cleanup (ADR 001: Sync-before-ACK / Crash-Recovery)
        let shutdown_span = info_span!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "persistence_shutdown"
        );
        async {
            info!("gRPC server stopped. Flushing databases to disk...");
            system_db.flush_async().await.map_err(anyhow::Error::from)?;
            log_db.flush_async().await.map_err(anyhow::Error::from)?;
            fsm_db.flush_async().await.map_err(anyhow::Error::from)?;
            info!("Databases synchronized successfully.");
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
fn create_deterministic_rng(node_id: common::types::NodeId) -> rand::rngs::StdRng {
    let mut seed = [0u8; 32];
    // Mix in the NodeId to guarantee uniqueness between nodes
    let node_id_bytes = node_id.as_u64().to_be_bytes();
    seed[0..8].copy_from_slice(&node_id_bytes);

    // Mix in OS entropy for cross-restart uniqueness
    rand::rng().fill(&mut seed[8..]);

    rand::rngs::StdRng::from_seed(seed)
}
