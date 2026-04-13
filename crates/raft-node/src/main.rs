mod config;
mod consensus;
mod identity;
mod node;
mod peer;
mod service;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use common::proto::v1::consensus_service_server::ConsensusServiceServer;
use common::proto::v1::ingress_service_server::IngressServiceServer;
use config::Config;
use consensus::spawn_election_timer;
use consensus::spawn_heartbeat_task;
use identity::NodeIdentity;
use identity::initialize_node_identity;
use node::Follower;
use node::RaftNode;
use node::RaftNodeState;
use peer::PeerManager;
use service::consensus::ConsensusDispatcher;
use service::ingress::IngressDispatcher;
use tokio::sync::RwLock;
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
            return Err(e);
        }
    };

    // 4. Initialize Persistence (sled)
    info!("Opening database at: {}", config.data_dir);
    let db = sled::open(&config.data_dir)?;

    // 5. Verify or Initialize Identity (ADR 004)
    let identity = match initialize_node_identity(&db, &config) {
        Ok(id) => Arc::new(id),
        Err(e) => {
            error!("Fatal Error during identity verification: {}", e);
            return Err(e);
        }
    };

    // 6. Initialize the Shared Node State (Type-State Engine)
    let initial_node = RaftNode::<Follower>::new(identity.clone());
    let shared_state = Arc::new(RwLock::new(RaftNodeState::Follower(initial_node)));

    // 7. Initialize RPC Service Dispatchers
    let consensus_dispatcher = ConsensusDispatcher::new(identity.clone(), shared_state.clone());
    let ingress_dispatcher = IngressDispatcher::new(identity.clone(), shared_state.clone());

    // 8. Initialize Peer Manager (Outbound Registry)
    let peer_manager = Arc::new(PeerManager::new(identity.clone(), &config.peers));

    // 9. Spawn Consensus Background Tasks (Election Timer & Heartbeats)
    spawn_election_timer(config.clone(), shared_state.clone(), peer_manager.clone());
    spawn_heartbeat_task(config.clone(), shared_state.clone(), peer_manager.clone());

    // 10. Create the Root Node Span
    let root_span = info_span!(
        "node",
        cluster = %identity.cluster_id(),
        id = %identity.node_id()
    );

    async move {
        info!("Identity verified. Transport layer starting...");

        let addr = config.listen_addr;
        info!("Starting gRPC server on {}", addr);

        // Define the graceful shutdown signal
        let shutdown = async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install CTRL+C handler");
            info!("Shutdown signal received. Commencing graceful exit...");
        };

        // 11. Start the gRPC Server
        Server::builder()
            .add_service(ConsensusServiceServer::new(consensus_dispatcher))
            .add_service(IngressServiceServer::new(ingress_dispatcher))
            .serve_with_shutdown(addr, shutdown)
            .await?;

        // 12. Persistence Cleanup (ADR 001: Sync-before-ACK / Crash-Recovery)
        info!("gRPC server stopped. Flushing database to disk...");
        db.flush_async().await?;
        info!("Database synchronized successfully.");

        info!("Node lifecycle finished successfully. Goodbye.");
        Ok(())
    }
    .instrument(root_span)
    .await
}
