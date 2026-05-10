use common::types::NodeIdentity;
use sled::Db;
use thiserror::Error;
use tracing::error;
use tracing::info;

use crate::config::Config;

/// Constants defining the location of node identity in the persistent store.
/// Centralizing these here prevents collision hazards with other sled clients.
const IDENTITY_KEY: &[u8] = b"node_identity";
const IDENTITY_TREE: &str = "_system_metadata";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("Persistence failure: {0}")]
    Storage(#[from] sled::Error),

    #[error("Identity metadata corruption: {0}")]
    Corruption(#[from] serde_json::Error),

    #[error("Safety Violation: Config ID ({config}) does not match Disk ID ({disk})")]
    Mismatch { disk: String, config: String },
}

/// Orchestrates the initialization and verification of the node's persistent
/// identity.
///
/// This is a "Bootstrap Guard" that enforces ADR 004: identity must be
/// immutable across restarts to prevent data corruption.
pub fn initialize_node_identity(db: &Db, config: &Config) -> Result<NodeIdentity, IdentityError> {
    let tree = db.open_tree(IDENTITY_TREE)?;

    match tree.get(IDENTITY_KEY)? {
        Some(bytes) => {
            let existing: NodeIdentity = serde_json::from_slice(&bytes)?;
            if existing.cluster_id() != &config.cluster_id || existing.node_id() != config.node_id {
                let config_id = format!("({}, {})", config.cluster_id, config.node_id);
                let disk_id = format!("({}, {})", existing.cluster_id(), existing.node_id());

                error!(
                    "IDENTITY MISMATCH: Config {} does not match Disk {}",
                    config_id, disk_id
                );
                return Err(IdentityError::Mismatch {
                    disk: disk_id,
                    config: config_id,
                });
            }
            info!(
                "Identity verified: Cluster={}, NodeID={}",
                existing.cluster_id(),
                existing.node_id()
            );
            Ok(existing)
        }
        None => {
            let identity = NodeIdentity::new(config.cluster_id.clone(), config.node_id);
            let bytes = serde_json::to_vec(&identity)?;
            tree.insert(IDENTITY_KEY, bytes)?;
            tree.flush()?; // Ensure fsync
            info!(
                "New identity persisted to disk: Cluster={}, NodeID={}",
                identity.cluster_id(),
                identity.node_id()
            );
            Ok(identity)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::Result;
    use common::types::ClusterId;
    use common::types::NodeId;

    use super::*;

    fn mock_config(cluster_id: &str, node_id: u64) -> Config {
        Config {
            cluster_id: ClusterId::try_new(cluster_id).unwrap(),
            node_id: NodeId::new(node_id),
            listen_addr: "127.0.0.1:50051".parse().unwrap(),
            data_dir: "".into(),
            peers: HashMap::new(),
            raft: Default::default(),
            policy: Default::default(),
        }
    }

    mod initialize_node_identity {
        use super::*;

        #[test]
        fn returns_new_identity_when_none_exists() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            let id = initialize_node_identity(&db, &config)?;

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
            initialize_node_identity(&db, &config)?;

            // Verification
            let id = initialize_node_identity(&db, &config)?;
            assert_eq!(
                id.cluster_id(),
                &ClusterId::try_new("test-cluster").unwrap()
            );
            assert_eq!(id.node_id(), NodeId::new(1));
            Ok(())
        }

        #[test]
        fn returns_mismatch_error_when_cluster_id_mismatches() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            // Initial setup
            initialize_node_identity(&db, &config)?;

            // Attempt with mismatch
            let mismatch_config = mock_config("wrong-cluster", 1);
            let result = initialize_node_identity(&db, &mismatch_config);

            assert!(matches!(result, Err(IdentityError::Mismatch { .. })));
            Ok(())
        }

        #[test]
        fn returns_mismatch_error_when_node_id_mismatches() -> Result<()> {
            let db = sled::Config::new().temporary(true).open()?;
            let config = mock_config("test-cluster", 1);

            // Initial setup
            initialize_node_identity(&db, &config)?;

            // Attempt with mismatch
            let mismatch_config = mock_config("test-cluster", 2);
            let result = initialize_node_identity(&db, &mismatch_config);

            assert!(matches!(result, Err(IdentityError::Mismatch { .. })));
            Ok(())
        }
    }
}
