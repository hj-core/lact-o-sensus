mod config;
mod identity;
mod node;
mod service;

use anyhow::Result;
use config::Config;
use identity::NodeIdentity;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize logging with EnvFilter (default to INFO)
    // This allows overrides like RUST_LOG=raft_node=debug,tonic=warn
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
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration from {}: {}", config_path, e);
            return Err(e);
        }
    };

    // 3. Initialize Persistence (sled)
    info!("Opening database at: {}", config.data_dir);
    let db = sled::open(&config.data_dir)?;

    // 4. Verify or Initialize Identity (ADR 004)
    if let Err(e) = NodeIdentity::initialize_or_verify(&db, &config) {
        error!("Fatal Error: {}", e);
        return Err(e);
    }

    // 5. Create the Root Node Span
    // All subsequent logic is wrapped in this span to ensure node identity
    // is included in every structured log entry.
    let root_span = info_span!(
        "node",
        cluster = %config.cluster_id,
        id = config.node_id
    );

    async move {
        info!("Node lifecycle started successfully.");

        // TODO: Phase 2, Task 2 - Implement gRPC Service Traits
        // TODO: Phase 2, Task 3 - Start gRPC Server
        // (Server::builder().add_service(...).serve(...))

        // Keep the node running (placeholder for server execution)
        tokio::signal::ctrl_c().await?;
        info!("Shutdown signal received. Commencing graceful exit.");

        Ok(())
    }
    .instrument(root_span)
    .await
}
