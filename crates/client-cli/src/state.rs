use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use common::types::ClientId;
use common::types::ClusterId;
use serde::Deserialize;
use serde::Serialize;

/// The maximum number of node addresses tracked in the client state.
pub const MAX_KNOWN_NODES: usize = 10;

/// Represents the persistent state of the Lact-O-Sensus client.
///
/// This state is critical for maintaining Exactly-Once Semantics (EOS)
/// and ensuring that mutations are not duplicated across retries or crashes.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientState {
    cluster_id: ClusterId,
    client_id: ClientId,
    sequence_id: u64,
    /// Capped list of known node addresses, prioritized by recency of success.
    known_nodes: Vec<String>,
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
    /// - If file exists: Validates `cluster_id`, merges `overriding_nodes`
    ///   (prioritizing them).
    /// - If file is missing: Requires at least one entry in `overriding_nodes`
    ///   to bootstrap.
    pub fn load_or_init<P: AsRef<Path>>(
        path: P,
        cluster_id: ClusterId,
        mut overriding_nodes: Vec<String>,
    ) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();

        // Defensive: truncate input early if it's too large
        overriding_nodes.truncate(MAX_KNOWN_NODES);

        if path_buf.exists() {
            let data = std::fs::read_to_string(&path_buf)
                .with_context(|| format!("Failed to read state file at {:?}", path_buf))?;
            let mut state: ClientState = serde_json::from_str(&data)
                .with_context(|| "Failed to deserialize client state")?;

            if state.cluster_id != cluster_id {
                anyhow::bail!(
                    "Cluster ID mismatch: expected {}, found {} in state file.",
                    cluster_id,
                    state.cluster_id
                );
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
                anyhow::bail!(
                    "Bootstrap failed: no state file found and no node addresses provided."
                );
            }

            let state = Self {
                cluster_id,
                client_id: ClientId::generate(),
                sequence_id: 0,
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
    pub fn next_sequence_id(&mut self) -> Result<u64> {
        self.sequence_id += 1;
        self.save()
            .context("Failed to persist sequence_id increment")?;
        Ok(self.sequence_id)
    }

    /// Records a successful interaction with a node, moving it to the front of
    /// the list.
    ///
    /// If the node is already at the front, this is a no-op to avoid
    /// unnecessary disk I/O.
    pub fn record_success(&mut self, node_addr: &str) -> Result<()> {
        if self.known_nodes.first().map(|s| s.as_str()) == Some(node_addr) {
            return Ok(());
        }

        let addr = node_addr.to_string();
        self.known_nodes.retain(|n| n != &addr);
        self.known_nodes.insert(0, addr);
        self.known_nodes.truncate(MAX_KNOWN_NODES);
        self.save()
            .context("Failed to persist successful node update")
    }

    /// Records a new leader hint received from a node.
    pub fn record_hint(&mut self, leader_addr: String) -> Result<()> {
        self.record_success(&leader_addr)
    }

    // --- Getters ---

    pub fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub fn sequence_id(&self) -> u64 {
        self.sequence_id
    }

    pub fn known_nodes(&self) -> &[String] {
        &self.known_nodes
    }

    // --- Internals ---

    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(self)
            .with_context(|| "Failed to serialize client state")?;
        std::fs::write(&self.path, data)
            .with_context(|| format!("Failed to write state file at {:?}", self.path))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    mod load_or_init {
        use super::*;

        #[test]
        fn initializes_new_state_when_file_missing() -> Result<()> {
            let dir = tempdir()?;
            let path = dir.path().join("state.json");
            let cluster_id = ClusterId::try_new("test-cluster")?;
            let nodes = vec!["127.0.0.1:50051".to_string()];

            let state = ClientState::load_or_init(&path, cluster_id.clone(), nodes.clone())?;
            assert_eq!(state.cluster_id(), &cluster_id);
            assert_eq!(state.known_nodes(), &nodes);
            assert_eq!(state.sequence_id(), 0);
            assert!(path.exists());

            Ok(())
        }

        #[test]
        fn merges_and_prioritizes_overriding_nodes() -> Result<()> {
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
        fn fails_on_cluster_id_mismatch() -> Result<()> {
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

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("mismatch"));

            Ok(())
        }
    }

    mod record_success {
        use super::*;

        #[test]
        fn moves_successful_node_to_front() -> Result<()> {
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
        fn performs_no_op_if_node_is_already_front() -> Result<()> {
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

    mod known_nodes_capping {
        use super::*;

        #[test]
        fn truncates_both_input_and_loaded_state() -> Result<()> {
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
                sequence_id: 0,
                known_nodes: large_nodes,
                path: path.clone(),
            };
            state.save()?;

            // 2. Load it - should be truncated to 10
            let loaded = ClientState::load_or_init(&path, cluster_id, vec![])?;
            assert_eq!(loaded.known_nodes().len(), 10);

            Ok(())
        }
    }
}
