use anyhow::Result;
use anyhow::anyhow;
use common::types::ClusterId;
use common::types::NodeId;
use serde::Deserialize;
use serde::Serialize;
use sled::Db;
use tracing::error;
use tracing::info;

use crate::config::Config;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeIdentity {
    cluster_id: ClusterId,
    node_id: NodeId,
}

impl NodeIdentity {
    const KEY: &'static [u8] = b"node_identity";
    const TREE_NAME: &'static str = "_system_metadata";

    pub fn new(cluster_id: ClusterId, node_id: NodeId) -> Self {
        Self {
            cluster_id,
            node_id,
        }
    }

    pub fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// Initializes the node's identity on disk or verifies it against the
    /// provided configuration.
    ///
    /// Per ADR 004, the identity must be persistent and immutable across
    /// restarts.
    pub fn initialize_or_verify(db: &Db, config: &Config) -> Result<Self> {
        let tree = db.open_tree(Self::TREE_NAME)?;

        match tree.get(Self::KEY)? {
            Some(bytes) => {
                let existing: NodeIdentity = serde_json::from_slice(&bytes)?;
                if existing.cluster_id != config.cluster_id || existing.node_id != config.node_id {
                    error!(
                        "IDENTITY MISMATCH: Config({}, {}) does not match Disk({}, {})",
                        config.cluster_id, config.node_id, existing.cluster_id, existing.node_id
                    );
                    return Err(anyhow!(
                        "Node identity on disk does not match configuration. Refusing to start to \
                         prevent data corruption."
                    ));
                }
                info!(
                    "Identity verified: Cluster={}, NodeID={}",
                    existing.cluster_id, existing.node_id
                );
                Ok(existing)
            }
            None => {
                let identity = NodeIdentity {
                    cluster_id: config.cluster_id.clone(),
                    node_id: config.node_id,
                };
                let bytes = serde_json::to_vec(&identity)?;
                tree.insert(Self::KEY, bytes)?;
                tree.flush()?; // Ensure fsync
                info!(
                    "New identity persisted to disk: Cluster={}, NodeID={}",
                    identity.cluster_id, identity.node_id
                );
                Ok(identity)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn mock_config(cluster_id: &str, node_id: u64) -> Config {
        Config {
            cluster_id: ClusterId::try_new(cluster_id).unwrap(),
            node_id: NodeId::new(node_id),
            listen_addr: "127.0.0.1:50051".parse().unwrap(),
            data_dir: "".to_string(),
            peers: HashMap::new(),
            raft: Default::default(),
        }
    }

    mod initialize_or_verify {
        use super::*;

        #[test]
        fn returns_new_identity_when_none_exists() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            let id = NodeIdentity::initialize_or_verify(&db, &config)?;

            assert_eq!(
                id.cluster_id(),
                &ClusterId::try_new("test-cluster").unwrap()
            );
            assert_eq!(id.node_id(), NodeId::new(1));
            Ok(())
        }

        #[test]
        fn returns_existing_identity_when_matches() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            // Initial setup
            NodeIdentity::initialize_or_verify(&db, &config)?;

            // Verification
            let id = NodeIdentity::initialize_or_verify(&db, &config)?;
            assert_eq!(
                id.cluster_id(),
                &ClusterId::try_new("test-cluster").unwrap()
            );
            assert_eq!(id.node_id(), NodeId::new(1));
            Ok(())
        }

        #[test]
        fn returns_err_when_cluster_id_mismatches() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            // Initial setup
            NodeIdentity::initialize_or_verify(&db, &config)?;

            // Attempt with mismatch
            let mismatch_config = mock_config("wrong-cluster", 1);
            let result = NodeIdentity::initialize_or_verify(&db, &mismatch_config);

            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("does not match configuration")
            );
            Ok(())
        }

        #[test]
        fn returns_err_when_node_id_mismatches() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            // Initial setup
            NodeIdentity::initialize_or_verify(&db, &config)?;

            // Attempt with mismatch
            let mismatch_config = mock_config("test-cluster", 2);
            let result = NodeIdentity::initialize_or_verify(&db, &mismatch_config);

            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("does not match configuration")
            );
            Ok(())
        }
    }
}
