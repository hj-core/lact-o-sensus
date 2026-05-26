//! Client state persistence and Exactly-Once Semantics (EOS) management.
//!
//! This module manages the local persistent state of the Lact-O-Sensus client,
//! ensuring that mutation sequence IDs are advanced and persisted correctly to
//! support linearizable retries. It also maintains a recency-prioritized
//! discovery list of known cluster node addresses.

use std::path::Path;
use std::path::PathBuf;

use common::types::ClientId;
use common::types::ClusterId;
use common::types::SequenceId;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// The maximum number of node addresses tracked in the client state.
pub const MAX_KNOWN_NODES: usize = 10;

/// Errors associated with client state management.
#[derive(Debug, Error)]
pub enum ClientStateError {
    #[error("I/O failure during state persistence: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization failure: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Cluster ID mismatch: expected {expected}, found {found} in state file")]
    ClusterIdMismatch {
        expected: ClusterId,
        found: ClusterId,
    },

    #[error("Bootstrap failed: no state file found and no node addresses provided")]
    BootstrapMissingNodes,

    #[error("Invalid sequence ID operation: {0}")]
    SequenceIdError(#[from] common::types::errors::ConsensusError),

    #[error("Arithmetic overflow in sequence ID: {0}")]
    ArithmeticError(#[from] common::types::errors::ArithmeticError),
}

/// Represents the persistent state of the Lact-O-Sensus client.
///
/// This state is critical for maintaining Exactly-Once Semantics (EOS)
/// and ensuring that mutations are not duplicated across retries or crashes.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientState {
    cluster_id: ClusterId,
    client_id: ClientId,
    sequence_id: SequenceId,
    /// Capped list of known node addresses, prioritized by recency of success.
    known_nodes: Vec<String>,
    /// Internal reference to the persistence file on disk.
    #[serde(skip)]
    path: PathBuf,
}

impl ClientState {
    // --- Initialization ---

    /// Loads the client state from the specified path or initializes it if
    /// missing.
    ///
    /// # Arguments
    /// * `path` - Path to the persistence file (e.g., .client_state.json).
    /// * `cluster_id` - The expected cluster identifier.
    /// * `overriding_nodes` - Optional new nodes to add to the discovery list.
    ///
    /// # Behavior
    /// - If file exists: Validates `cluster_id`, merges `overriding_nodes` by
    ///   prepending them and deduplicating existing entries.
    /// - If file is missing: Requires at least one entry in `overriding_nodes`
    ///   to bootstrap.
    ///
    /// Addresses are automatically deduplicated during the merge; if an address
    /// is already known, it is moved to the front (highest priority).
    pub fn load_or_init<P: AsRef<Path>>(
        path: P,
        cluster_id: ClusterId,
        mut overriding_nodes: Vec<String>,
    ) -> Result<Self, ClientStateError> {
        let path_buf = path.as_ref().to_path_buf();

        // Defensive: truncate input early if it's too large
        overriding_nodes.truncate(MAX_KNOWN_NODES);

        if path_buf.exists() {
            let data = std::fs::read_to_string(&path_buf)?;
            let mut state: ClientState = serde_json::from_str(&data)?;

            if state.cluster_id != cluster_id {
                return Err(ClientStateError::ClusterIdMismatch {
                    expected: cluster_id,
                    found: state.cluster_id,
                });
            }

            state.path = path_buf;

            // Defensive: ensure loaded nodes are also within limits before merging
            state.known_nodes.truncate(MAX_KNOWN_NODES);

            // Merge new nodes: prepend and deduplicate
            if !overriding_nodes.is_empty() {
                for node in overriding_nodes.into_iter().rev() {
                    state.known_nodes.retain(|n| n != &node);
                    state.known_nodes.insert(0, node);
                }
                state.known_nodes.truncate(MAX_KNOWN_NODES);
                state.save()?;
            }

            Ok(state)
        } else {
            if overriding_nodes.is_empty() {
                return Err(ClientStateError::BootstrapMissingNodes);
            }

            let state = Self {
                cluster_id,
                client_id: ClientId::generate(),
                sequence_id: SequenceId::ZERO,
                known_nodes: overriding_nodes,
                path: path_buf,
            };
            state.save()?;
            Ok(state)
        }
    }

    // --- Mutators ---

    /// Increments the sequence ID and persists the change to disk.
    ///
    /// This MUST be called before issuing a new mutation request to ensure
    /// Exactly-Once Semantics across client restarts.
    pub fn next_sequence_id(&mut self) -> Result<SequenceId, ClientStateError> {
        self.sequence_id = (self.sequence_id + 1)?;
        self.save()?;
        Ok(self.sequence_id)
    }

    /// Records a successful interaction with a node, moving it to the front of
    /// the list.
    ///
    /// If the node is already at the front, this is a no-op to avoid
    /// unnecessary disk I/O.
    pub fn record_success(&mut self, node_addr: &str) -> Result<(), ClientStateError> {
        if self.known_nodes.first().map(|s| s.as_str()) == Some(node_addr) {
            return Ok(());
        }

        let addr = node_addr.to_string();
        self.known_nodes.retain(|n| n != &addr);
        self.known_nodes.insert(0, addr);
        self.known_nodes.truncate(MAX_KNOWN_NODES);
        self.save()
    }

    /// Records a new leader hint received from a node.
    pub fn record_hint(&mut self, leader_addr: String) -> Result<(), ClientStateError> {
        self.record_success(&leader_addr)
    }

    /// Rotates the known nodes list, moving the current primary to the back.
    ///
    /// This is used when a node is unreachable or consistently fails to provide
    /// a valid leader hint.
    pub fn rotate_nodes(&mut self) -> Result<(), ClientStateError> {
        if self.known_nodes.len() > 1 {
            let current = self.known_nodes.remove(0);
            self.known_nodes.push(current);
            self.save()?;
        }
        Ok(())
    }

    // --- Getters ---

    pub fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub fn sequence_id(&self) -> SequenceId {
        self.sequence_id
    }

    pub fn known_nodes(&self) -> &[String] {
        &self.known_nodes
    }

    // --- Internals ---

    fn save(&self) -> Result<(), ClientStateError> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    mod load_or_init {
        use super::*;

        mod with_fresh_start {
            use super::*;

            #[test]
            fn initializes_new_state_when_file_missing() -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;
                let nodes = vec!["127.0.0.1:50051".to_string()];

                let state = ClientState::load_or_init(&path, cluster_id.clone(), nodes.clone())?;
                assert_eq!(state.cluster_id(), &cluster_id);
                assert_eq!(state.known_nodes(), &nodes);
                assert_eq!(state.sequence_id(), SequenceId::ZERO);
                assert!(path.exists());

                Ok(())
            }

            #[test]
            fn returns_bootstrap_error_when_no_nodes_provided()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;

                let result = ClientState::load_or_init(&path, cluster_id, vec![]);
                assert!(matches!(
                    result,
                    Err(ClientStateError::BootstrapMissingNodes)
                ));

                Ok(())
            }
        }

        mod with_existing_state {
            use super::*;

            #[test]
            fn merges_and_prioritizes_overriding_nodes_when_provided()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;

                // 1. Initial save
                ClientState::load_or_init(&path, cluster_id.clone(), vec!["node1".to_string()])?;

                // 2. Load with override
                let state = ClientState::load_or_init(
                    &path,
                    cluster_id,
                    vec!["node2".to_string(), "node1".to_string()],
                )?;

                assert_eq!(state.known_nodes()[0], "node2");
                assert_eq!(state.known_nodes()[1], "node1");
                assert_eq!(state.known_nodes().len(), 2);

                Ok(())
            }

            #[test]
            fn deduplicates_addresses_when_merging_overrides()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;

                // 1. Initial state: [node1]
                ClientState::load_or_init(&path, cluster_id.clone(), vec!["node1".to_string()])?;

                // 2. Merge overrides containing node1 again: [node1, node2]
                let state = ClientState::load_or_init(
                    &path,
                    cluster_id,
                    vec!["node1".to_string(), "node2".to_string()],
                )?;

                assert_eq!(state.known_nodes().len(), 2);
                assert_eq!(state.known_nodes()[0], "node1");
                assert_eq!(state.known_nodes()[1], "node2");

                Ok(())
            }

            #[test]
            fn returns_error_when_cluster_id_mismatches() -> Result<(), Box<dyn std::error::Error>>
            {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");

                // 1. Init with cluster A
                ClientState::load_or_init(
                    &path,
                    ClusterId::try_new("cluster-A")?,
                    vec!["node1".to_string()],
                )?;

                // 2. Load with cluster B
                let result = ClientState::load_or_init(
                    &path,
                    ClusterId::try_new("cluster-B")?,
                    vec!["node1".to_string()],
                );

                assert!(matches!(
                    result,
                    Err(ClientStateError::ClusterIdMismatch { .. })
                ));

                Ok(())
            }
        }
    }

    mod record_success {
        use super::*;

        mod address_prioritization {
            use super::*;

            #[test]
            fn moves_successful_node_to_front_when_recorded()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;
                let mut state = ClientState::load_or_init(
                    &path,
                    cluster_id,
                    vec!["node1".to_string(), "node2".to_string()],
                )?;

                state.record_success("node2")?;
                assert_eq!(state.known_nodes()[0], "node2");

                Ok(())
            }

            #[test]
            fn skips_disk_write_when_node_is_already_at_front()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;
                let mut state =
                    ClientState::load_or_init(&path, cluster_id, vec!["node1".to_string()])?;

                let metadata_before = std::fs::metadata(&path)?;
                // Sleep to ensure potential mtime change is detectable
                std::thread::sleep(std::time::Duration::from_millis(10));

                state.record_success("node1")?;

                let metadata_after = std::fs::metadata(&path)?;
                assert_eq!(
                    metadata_before.modified()?,
                    metadata_after.modified()?,
                    "Disk write occurred for redundant success record"
                );

                Ok(())
            }
        }
    }

    mod known_nodes_capping {
        use super::*;

        mod storage_limits {
            use super::*;

            #[test]
            fn truncates_node_list_to_maximum_allowed_when_loading_bloated_file()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let path = dir.path().join("state.json");
                let cluster_id = ClusterId::try_new("test-cluster")?;

                // 1. Manually create a "bloated" state file
                let mut large_nodes = Vec::new();
                for i in 0..20 {
                    large_nodes.push(format!("node{}", i));
                }

                let state = ClientState {
                    cluster_id: cluster_id.clone(),
                    client_id: ClientId::generate(),
                    sequence_id: SequenceId::ZERO,
                    known_nodes: large_nodes,
                    path: path.clone(),
                };
                // Use standard save logic to bypass truncate checks during struct construction
                let data = serde_json::to_string_pretty(&state)?;
                std::fs::write(&path, data)?;

                // 2. Load it - should be truncated to 10
                let loaded = ClientState::load_or_init(&path, cluster_id, vec![])?;
                assert_eq!(loaded.known_nodes().len(), MAX_KNOWN_NODES);

                Ok(())
            }
        }
    }
}
