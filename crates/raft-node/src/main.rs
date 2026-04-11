mod config;
mod identity;
mod node;
mod peer;
mod service;

use std::sync::Arc;

use anyhow::Result;
use common::proto::v1::consensus_service_server::ConsensusServiceServer;
use common::proto::v1::ingress_service_server::IngressServiceServer;
use config::Config;
use identity::NodeIdentity;
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

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize logging with EnvFilter (default to INFO)
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    info!("Lact-O-Sensus Node Initializing...");

    // 2. Load Configuration
    let config_path = "config.toml";
    let config = match Config::load(config_path) {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            error!("Failed to load configuration from {}: {}", config_path, e);
            return Err(e);
        }
    };

    // 3. Initialize Persistence (sled)
    info!("Opening database at: {}", config.data_dir);
    let db = sled::open(&config.data_dir)?;

    // 4. Verify or Initialize Identity (ADR 004)
    let identity = match NodeIdentity::initialize_or_verify(&db, &config) {
        Ok(id) => Arc::new(id),
        Err(e) => {
            error!("Fatal Error during identity verification: {}", e);
            return Err(e);
        }
    };

    // 5. Initialize the Shared Node State (Type-State Engine)
    let initial_node = RaftNode::<Follower>::new(identity.clone());
    let shared_state = Arc::new(RwLock::new(RaftNodeState::Follower(initial_node)));

    // 6. Initialize RPC Service Dispatchers
    let consensus_dispatcher = ConsensusDispatcher::new(identity.clone(), shared_state.clone());
    let ingress_dispatcher = IngressDispatcher::new(identity.clone(), shared_state.clone());

    // 7. Initialize Peer Manager (Outbound Registry)
    let _peer_manager = Arc::new(PeerManager::new(identity.clone(), &config.peers));

    // 8. Create the Root Node Span
    let root_span = info_span!(
        "node",
        cluster = %identity.cluster_id,
        id = identity.node_id
    );

    async move {
        info!("Identity verified. Transport layer starting...");

        let addr = config.listen_addr;
        info!("Starting gRPC server on {}", addr);

        // Define the graceful shutdown signal (Task 3.3 preview)
        let shutdown = async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install CTRL+C handler");
            info!("Shutdown signal received. Starting graceful exit...");
        };

        // 8. Start the gRPC Server
        Server::builder()
            .add_service(ConsensusServiceServer::new(consensus_dispatcher))
            .add_service(IngressServiceServer::new(ingress_dispatcher))
            .serve_with_shutdown(addr, shutdown)
            .await?;

        info!("Node lifecycle finished successfully.");
        Ok(())
    }
    .instrument(root_span)
    .await
}
