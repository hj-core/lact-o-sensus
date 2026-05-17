use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use common::proto::v1::app::ingress_service_server::IngressServiceServer;
use common::proto::v1::raft::consensus_service_server::ConsensusServiceServer;
use common::rpc::IdentityInterceptor;
use gateway::ingress::IngressDispatcher;
use gateway::veto::GrpcVetoRelay;
use lacto_fsm::LactoStore;
use raft_engine::config::Config;
use raft_engine::consensus::spawn_election_timer;
use raft_engine::consensus::spawn_heartbeat_task;
use raft_engine::engine::Follower;
use raft_engine::engine::LogicalNode;
use raft_engine::identity::initialize_node_identity;
use raft_engine::node::RaftNode;
use raft_engine::peer::PeerManager;
use raft_engine::recovery::RecoveryManager;
use raft_engine::service::consensus::ConsensusDispatcher;
use raft_engine::service::handle::LocalRaftHandle;
use raft_engine::shell::ConsensusShell;
use raft_engine::storage::SledStorage;
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

    info!("Lact-O-Sensus Node Initializing...");

    // 3. Load Configuration
    let config = match Config::load(&args.config) {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            error!("Failed to load configuration from {:?}: {}", args.config, e);
            return Err(e.into());
        }
    };

    // 4. Initialize Isolated Persistence (sled) (ADR 001/009)
    // Establishing split databases for strict component isolation.
    let system_path = config.data_dir.join("system");
    let log_path = config.data_dir.join("log");
    let fsm_path = config.data_dir.join("fsm");

    info!("Opening system database at: {}", system_path.display());
    let system_db = sled::open(&system_path).map_err(anyhow::Error::from)?;

    info!("Opening log database at: {}", log_path.display());
    let log_db = sled::open(&log_path).map_err(anyhow::Error::from)?;

    info!("Opening FSM database at: {}", fsm_path.display());
    let fsm_db = sled::open(&fsm_path).map_err(anyhow::Error::from)?;

    // 5. Verify or Initialize Identity (ADR 004)
    let identity = match initialize_node_identity(&system_db, &config) {
        Ok(id) => Arc::new(id),
        Err(e) => {
            error!("Fatal Error during identity verification: {}", e);
            return Err(e.into());
        }
    };

    // 6. Initialize the Shared Node State (Atomic Shell)
    let fsm_store = LactoStore::new(fsm_db.clone())
        .map_err(|e| anyhow::anyhow!("Failed to initialize LactoStore: {}", e))?;
    let fsm = Arc::new(fsm_store);
    let storage = Arc::new(
        SledStorage::new(log_db.clone())
            .map_err(|e| anyhow::anyhow!("Failed to initialize SledStorage: {}", e))?,
    );

    // 6.1 Cold-Boot Recovery (ADR 001/009)
    // Synchronize FSM with Consensus Log before accepting any network events.
    let recovery = RecoveryManager::new(fsm.clone(), storage.clone());
    recovery
        .recover()
        .await
        .map_err(|e| anyhow::anyhow!("Cold-boot recovery failed: {}", e))?;

    let initial_node =
        RaftNode::<Follower>::try_new(identity.clone(), fsm.clone(), storage.clone())
            .map_err(|e| anyhow::anyhow!("Failed to initialize RaftNode: {}", e))?;
    let shared_state = Arc::new(ConsensusShell::new(LogicalNode::Follower(initial_node)));

    // 7. Initialize Networking (Outbound Peer Mesh)
    let peer_manager = Arc::new(match PeerManager::new(identity.clone(), &config.peers) {
        Ok(m) => m,
        Err(e) => {
            error!("Fatal Error during Peer Manager initialization: {}", e);
            return Err(e.into());
        }
    });

    // 8. Initialize RPC Service Dispatchers
    let consensus_dispatcher = ConsensusDispatcher::new(identity.clone(), shared_state.clone());

    // Initialize the Raft Handle for the Gateway (ADR 005/007)
    let raft_handle = Arc::new(LocalRaftHandle::new(
        shared_state.clone(),
        peer_manager.clone(),
    ));

    // Initialize the AI Veto Relay (Egress Bridge)
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

    // 9. Spawn Consensus Background Tasks (Election Timer & Heartbeats)
    spawn_election_timer(config.clone(), shared_state.clone(), peer_manager.clone());
    spawn_heartbeat_task(config.clone(), shared_state.clone(), peer_manager.clone());

    // 10. Create the Root Node Span
    let root_span = info_span!(
        "node",
        cluster = %identity.cluster_id(),
        id = %identity.node_id()
    );

    let interceptor = IdentityInterceptor::new(identity.clone());

    async move {
        info!("Identity verified. Transport layer starting...");

        let addr = config.listen_addr;
        info!("Starting gRPC server on {}", addr);

        // Define the graceful shutdown signal
        let shutdown = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Failed to install CTRL+C handler: {}", e);
            } else {
                info!("Shutdown signal received. Commencing graceful exit...");
            }
        };

        // 11. Start the gRPC Server
        Server::builder()
            .add_service(ConsensusServiceServer::with_interceptor(
                consensus_dispatcher,
                interceptor.clone(),
            ))
            .add_service(IngressServiceServer::with_interceptor(
                ingress_dispatcher,
                interceptor,
            ))
            .serve_with_shutdown(addr, shutdown)
            .await
            .map_err(anyhow::Error::from)?;

        // 12. Persistence Cleanup (ADR 001: Sync-before-ACK / Crash-Recovery)
        info!("gRPC server stopped. Flushing databases to disk...");
        system_db.flush_async().await.map_err(anyhow::Error::from)?;
        log_db.flush_async().await.map_err(anyhow::Error::from)?;
        fsm_db.flush_async().await.map_err(anyhow::Error::from)?;
        info!("Databases synchronized successfully.");

        info!("Node lifecycle finished successfully. Goodbye.");
        Ok(())
    }
    .instrument(root_span)
    .await
}
