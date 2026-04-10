mod config;

use anyhow::Result;
use config::Config;
use tracing::Level;
use tracing::info;
use tracing::warn;
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Lact-O-Sensus Node Starting...");

    // Basic config loading test (Phase 2, Task 1.1)
    // In a real scenario, the path would be passed via CLI args.
    let config_path = "config.toml";
    match Config::load(config_path) {
        Ok(cfg) => {
            info!(
                "Config loaded successfully. Cluster: {}, Node ID: {}, Listening on: {}",
                cfg.cluster_id, cfg.node_id, cfg.listen_addr
            );
        }
        Err(e) => {
            warn!(
                "Failed to load config from {}: {}. Using defaults or exiting.",
                config_path, e
            );
        }
    }

    Ok(())
}
