mod config;
mod identity;

use anyhow::Result;
use config::Config;
use identity::NodeIdentity;
use tracing::Level;
use tracing::error;
use tracing::info;
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Lact-O-Sensus Node Starting...");

    // 1. Load Configuration
    let config_path = "config.toml";
    let config = match Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration from {}: {}", config_path, e);
            return Err(e);
        }
    };

    // 2. Initialize Persistence (sled)
    info!("Opening database at: {}", config.data_dir);
    let db = sled::open(&config.data_dir)?;

    // 3. Verify or Initialize Identity (ADR 004)
    if let Err(e) = NodeIdentity::initialize_or_verify(&db, &config) {
        error!("Fatal Error: {}", e);
        return Err(e);
    }

    info!(
        "Node {} (Cluster: {}) initialized successfully.",
        config.node_id, config.cluster_id
    );

    // TODO: Phase 2, Task 3 - Start gRPC Server

    Ok(())
}
