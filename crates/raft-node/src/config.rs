use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use anyhow::anyhow;
use common::types::ClusterId;
use common::types::NodeId;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Unique identifier for the entire consensus group.
    pub cluster_id: ClusterId,

    /// Unique identifier for this specific node within its cluster.
    pub node_id: NodeId,

    /// Network address to listen on for gRPC traffic.
    pub listen_addr: SocketAddr,

    /// Directory for persistent storage (sled db).
    pub data_dir: String,

    /// Mapping of node IDs to their network addresses for all peers.
    pub peers: HashMap<NodeId, SocketAddr>,
}

impl Config {
    /// Loads the configuration from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;

        config.validate()?;

        Ok(config)
    }

    /// Performs basic validation of the configuration.
    fn validate(&self) -> Result<()> {
        if self.peers.contains_key(&self.node_id) {
            return Err(anyhow!(
                "Self-loop detected: node_id {} found in peers list",
                self.node_id
            ));
        }

        if self.peers.is_empty() {
            warn!("Configuration loaded with 0 peers. This node will be a single-node cluster.");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod validate {
        use super::*;

        #[test]
        fn returns_ok_when_valid_config() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "127.0.0.1:50052"
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn returns_err_when_self_loop() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            1 = "127.0.0.1:50051"
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_err());
        }

        #[test]
        fn returns_err_when_empty_cluster_id() {
            let toml_str = r#"
            cluster_id = ""
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "127.0.0.1:50052"
        "#;
            // This will now fail during deserialization because of #[serde(try_from)]
            let result: Result<Config, _> = toml::from_str(toml_str);
            assert!(result.is_err());
        }
    }
}
