use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use common::types::ClusterId;
use common::types::NodeId;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read configuration file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("Self-loop detected: node_id {0} found in peers list")]
    SelfLoop(NodeId),

    #[error("Invalid Raft timing: {0}")]
    TimingInvariant(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Unique identifier for the entire consensus group.
    pub cluster_id: ClusterId,

    /// Unique identifier for this specific node within its cluster.
    pub node_id: NodeId,

    /// Network address to listen on for gRPC traffic.
    pub listen_addr: SocketAddr,

    /// Directory for persistent storage (sled db).
    pub data_dir: PathBuf,

    /// Mapping of node IDs to their network addresses for all peers.
    pub peers: HashMap<NodeId, SocketAddr>,

    /// Raft-specific timing and protocol configuration.
    #[serde(default)]
    pub raft: RaftConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RaftConfig {
    /// Interval between heartbeats sent by the leader (in milliseconds).
    pub heartbeat_interval_ms: u64,

    /// Minimum election timeout (in milliseconds).
    pub election_timeout_min_ms: u64,

    /// Maximum election timeout (in milliseconds).
    pub election_timeout_max_ms: u64,

    /// Timeout for internal peer-to-peer RPC calls (in milliseconds).
    pub rpc_timeout_ms: u64,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 300,
            rpc_timeout_ms: 40,
        }
    }
}

impl RaftConfig {
    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_millis(self.heartbeat_interval_ms)
    }

    pub fn election_timeout_min(&self) -> Duration {
        Duration::from_millis(self.election_timeout_min_ms)
    }

    pub fn election_timeout_max(&self) -> Duration {
        Duration::from_millis(self.election_timeout_max_ms)
    }

    pub fn rpc_timeout(&self) -> Duration {
        Duration::from_millis(self.rpc_timeout_ms)
    }

    /// Validates Raft-specific timing invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.heartbeat_interval_ms == 0 {
            return Err(ConfigError::TimingInvariant(
                "heartbeat_interval_ms must be greater than 0".to_string(),
            ));
        }

        if self.rpc_timeout_ms == 0 {
            return Err(ConfigError::TimingInvariant(
                "rpc_timeout_ms must be greater than 0".to_string(),
            ));
        }

        if self.election_timeout_min_ms <= self.heartbeat_interval_ms {
            return Err(ConfigError::TimingInvariant(
                "election_timeout_min_ms must be greater than heartbeat_interval_ms".to_string(),
            ));
        }

        if self.election_timeout_max_ms <= self.election_timeout_min_ms {
            return Err(ConfigError::TimingInvariant(
                "election_timeout_max_ms must be greater than election_timeout_min_ms".to_string(),
            ));
        }

        Ok(())
    }
}

impl Config {
    /// Loads the configuration from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;

        config.validate()?;

        Ok(config)
    }

    /// Performs basic validation of the configuration.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.peers.contains_key(&self.node_id) {
            return Err(ConfigError::SelfLoop(self.node_id));
        }

        // Delegate Raft timing validation
        self.raft.validate()?;

        if self.peers.is_empty() {
            warn!("Configuration loaded with 0 peers. This node will be a single-node cluster.");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod config_validate {
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
            let result = config.validate();
            assert!(matches!(result, Err(ConfigError::SelfLoop(_))));
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
            // This fails during deserialization because of #[serde(try_from)] on ClusterId
            let result: Result<Config, toml::de::Error> = toml::from_str(toml_str);
            assert!(result.is_err());
        }

        #[test]
        fn returns_err_when_invalid_raft_timing() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 100
            election_timeout_min_ms = 50
            election_timeout_max_ms = 200
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(result, Err(ConfigError::TimingInvariant(_))));
        }

        #[test]
        fn returns_ok_when_partial_raft_timing() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 100
            # election_timeout fields missing, should default
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
            assert_eq!(config.raft.heartbeat_interval_ms, 100);
            assert_eq!(config.raft.election_timeout_min_ms, 150); // Default
        }
    }
}
