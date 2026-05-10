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

    #[error("Invalid configuration invariant: {0}")]
    TimingInvariant(String),

    #[error("Invalid URI: {0}")]
    InvalidUri(String),
}

// --- Raft Protocol Configuration ---

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

        // The Congestion Invariant: RPC timeout must be shorter than heartbeat
        // interval to prevent heartbeat stacking and network congestion.
        if self.rpc_timeout_ms >= self.heartbeat_interval_ms {
            return Err(ConfigError::TimingInvariant(
                "rpc_timeout_ms must be less than heartbeat_interval_ms to prevent heartbeat \
                 stacking"
                    .to_string(),
            ));
        }

        // The Stability Invariant (ADR 003): Heartbeat interval should be
        // significantly shorter than the minimum election timeout.
        // Specifically, maintaining a 1:3-1:6 ratio.
        if self.election_timeout_min_ms < self.heartbeat_interval_ms * 3 {
            return Err(ConfigError::TimingInvariant(
                "ADR 003: election_timeout_min_ms must be at least 3x heartbeat_interval_ms"
                    .to_string(),
            ));
        }

        if self.election_timeout_min_ms > self.heartbeat_interval_ms * 6 {
            warn!(
                "Wide heartbeat-to-election ratio ({}ms : {}ms). Liveness may be delayed beyond \
                 ADR 003 recommendations.",
                self.heartbeat_interval_ms, self.election_timeout_min_ms
            );
        }

        if self.election_timeout_max_ms <= self.election_timeout_min_ms {
            return Err(ConfigError::TimingInvariant(
                "election_timeout_max_ms must be greater than election_timeout_min_ms".to_string(),
            ));
        }

        Ok(())
    }
}

// --- External Policy Configuration ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// Network address of the AI Veto Node (e.g. "http://127.0.0.1:50060").
    pub veto_addr: String,

    /// Timeout for AI Veto evaluations (in milliseconds).
    pub veto_timeout_ms: u64,

    /// Maximum number of leader-internal retries for malformed AI responses
    /// or transient infrastructure failures.
    pub veto_max_retries: usize,

    /// Maximum characters allowed in the AI's moral justification.
    pub max_justification_len: usize,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            veto_addr: "http://127.0.0.1:50060".to_string(),
            veto_timeout_ms: 5000,
            veto_max_retries: 1,
            max_justification_len: 512,
        }
    }
}

impl PolicyConfig {
    pub fn veto_timeout(&self) -> Duration {
        Duration::from_millis(self.veto_timeout_ms)
    }

    /// Returns the veto address as a tonic-compatible Endpoint.
    /// Performs strict validation via Endpoint::from_shared.
    pub fn veto_endpoint(&self) -> Result<tonic::transport::Endpoint, ConfigError> {
        if self.veto_addr.is_empty() {
            return Err(ConfigError::InvalidUri(
                "veto_addr cannot be empty".to_string(),
            ));
        }

        tonic::transport::Endpoint::from_shared(self.veto_addr.clone()).map_err(|e| {
            ConfigError::InvalidUri(format!("veto_addr '{}' is invalid: {}", self.veto_addr, e))
        })
    }

    /// Validates policy-specific invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.veto_timeout_ms == 0 {
            return Err(ConfigError::TimingInvariant(
                "veto_timeout_ms must be greater than 0".to_string(),
            ));
        }

        // Delegate URI validation to the conversion logic
        self.veto_endpoint()?;

        Ok(())
    }
}

// --- Root Node Configuration ---

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
    pub peers: HashMap<NodeId, String>,

    /// Raft-specific timing and protocol configuration.
    #[serde(default)]
    pub raft: RaftConfig,

    /// Configuration for external policy evaluation (AI Veto).
    #[serde(default)]
    pub policy: PolicyConfig,
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

        // Validate all peer URIs
        for (id, uri) in &self.peers {
            if uri.is_empty() {
                return Err(ConfigError::InvalidUri(format!(
                    "Peer ID {} has an empty URI",
                    id
                )));
            }

            tonic::transport::Endpoint::from_shared(uri.clone()).map_err(|e| {
                ConfigError::InvalidUri(format!("Peer ID {} URI '{}' is invalid: {}", id, uri, e))
            })?;
        }

        // Delegate sub-config validation
        self.raft.validate()?;
        self.policy.validate()?;

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
        fn accept_valid_configuration() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn reject_config_when_node_id_is_in_peers_list() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            1 = "http://127.0.0.1:50051"
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(result, Err(ConfigError::SelfLoop(_))));
        }

        #[test]
        fn reject_config_when_cluster_id_is_empty() {
            let toml_str = r#"
            cluster_id = ""
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
        "#;
            // Fails during deserialization because of #[serde(try_from)] on ClusterId
            let result: Result<Config, toml::de::Error> = toml::from_str(toml_str);
            assert!(result.is_err());
        }

        #[test]
        fn reject_config_when_rpc_timeout_exceeds_heartbeat_interval() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 50
            rpc_timeout_ms = 60
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(
                result,
                Err(ConfigError::TimingInvariant(ref msg)) if msg.contains("Heartbeat") || msg.contains("stacking")
            ));
        }

        #[test]
        fn reject_config_when_election_timeout_is_below_3x_heartbeat() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 100
            election_timeout_min_ms = 299 # Violation of ADR 003
            election_timeout_max_ms = 400
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(
                result,
                Err(ConfigError::TimingInvariant(ref msg)) if msg.contains("ADR 003")
            ));
        }

        #[test]
        fn accept_config_when_election_timeout_is_exactly_3x_heartbeat() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 100
            election_timeout_min_ms = 300
            election_timeout_max_ms = 450
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn accept_config_with_warning_when_election_timeout_exceeds_6x_heartbeat() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 100
            election_timeout_min_ms = 700 # > 6x, should warn but pass
            election_timeout_max_ms = 900
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn use_default_protocol_settings_when_fields_are_unspecified() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [raft]
            heartbeat_interval_ms = 45 # Override one; must be > rpc_timeout (40ms default)
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
            assert_eq!(config.raft.heartbeat_interval_ms, 45);
            assert_eq!(config.raft.election_timeout_min_ms, 150); // Default (150 > 3*45)
            assert_eq!(config.raft.rpc_timeout_ms, 40); // Default
        }

        #[test]
        fn reject_config_when_veto_timeout_is_zero() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [policy]
            veto_timeout_ms = 0
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(
                result,
                Err(ConfigError::TimingInvariant(ref msg)) if msg.contains("veto_timeout_ms")
            ));
        }

        #[test]
        fn reject_config_when_peer_uri_is_malformed() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://invalid uri.com"
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(result, Err(ConfigError::InvalidUri(_))));
        }

        #[test]
        fn reject_config_when_veto_uri_is_malformed() {
            let toml_str = r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            [peers]
            2 = "http://127.0.0.1:50052"
            [policy]
            veto_addr = "http://invalid uri.com" # Spaces are strictly forbidden
        "#;
            let config: Config = toml::from_str(toml_str).unwrap();
            let result = config.validate();
            assert!(matches!(result, Err(ConfigError::InvalidUri(_))));
        }
    }
}
