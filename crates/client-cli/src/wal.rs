//! Client Write-Ahead Log (WAL) for mutation intent durability.
//!
//! This module provides the `IntentWal`, a disk-persistent log (backed by
//! `sled`) that ensures mutation intents are recorded before network dispatch.
//! This fulfills ADR 001 (Crash-Recovery) by enabling linearizable re-proposal
//! of unacknowledged intents after a client crash or restart.

use std::path::Path;

use common::proto::v1::app::ProposeMutationRequest;
use common::types::SequenceId;
use prost::Message;
use sled::Db;
use thiserror::Error;

/// Errors associated with WAL operations.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("Database failure: {0}")]
    Db(#[from] sled::Error),

    #[error("Data corruption: {0}")]
    Corruption(String),

    #[error("Serialization failure: {0}")]
    Serialization(String),
}

/// A Write-Ahead Log (WAL) for ensuring the durability of mutation intents.
pub struct IntentWal {
    db: Db,
}

impl IntentWal {
    /// Opens the WAL at the specified path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WalError> {
        let db = sled::open(path)?;
        Ok(Self { db })
    }

    /// Appends a new mutation intent to the WAL.
    ///
    /// This must be called BEFORE the RPC is dispatched to the cluster.
    pub fn append(
        &self,
        sequence_id: SequenceId,
        req: &ProposeMutationRequest,
    ) -> Result<(), WalError> {
        let key = sequence_id.as_u64().to_be_bytes();
        let value = req.encode_to_vec();

        self.db.insert(key, value)?;

        // Mandatory fsync before acknowledging persistence (ADR 001).
        self.db.flush()?;
        Ok(())
    }

    /// Removes an intent from the WAL.
    ///
    /// This should be called once the mutation has reached a terminal state
    /// (e.g., COMMITTED or VETOED).
    pub fn remove(&self, sequence_id: SequenceId) -> Result<(), WalError> {
        let key = sequence_id.as_u64().to_be_bytes();

        self.db.remove(key)?;
        self.db.flush()?;
        Ok(())
    }

    /// Recovers all pending intents from the WAL.
    ///
    /// Returns intents sorted by sequence ID to maintain absolute temporal
    /// ordering during recovery.
    pub fn recover(&self) -> Result<Vec<(SequenceId, ProposeMutationRequest)>, WalError> {
        let mut recovered = Vec::new();

        for item in self.db.iter() {
            let (key, value) = item?;
            let entry = self.decode_entry(key, value)?;
            recovered.push(entry);
        }

        // Ensure monotonic recovery order.
        recovered.sort_by_key(|(seq, _)| seq.as_u64());
        Ok(recovered)
    }

    /// Internal helper to decode a WAL entry from raw bytes.
    fn decode_entry(
        &self,
        key: sled::IVec,
        value: sled::IVec,
    ) -> Result<(SequenceId, ProposeMutationRequest), WalError> {
        let seq_val = u64::from_be_bytes(
            key.as_ref()
                .try_into()
                .map_err(|_| WalError::Corruption("Invalid sequence ID key length".into()))?,
        );

        let req = ProposeMutationRequest::decode(value.as_ref())
            .map_err(|e| WalError::Serialization(e.to_string()))?;

        Ok((SequenceId::new(seq_val), req))
    }
}

#[cfg(test)]
mod tests {
    use common::proto::v1::app::MutationIntent;
    use common::proto::v1::app::OperationType;
    use common::types::ClientId;
    use tempfile::tempdir;

    use super::*;

    fn mock_request(seq: u64) -> ProposeMutationRequest {
        ProposeMutationRequest::new(
            &ClientId::generate(),
            SequenceId::new(seq),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        )
    }

    mod open {
        use super::*;

        mod with_new_path {
            use super::*;

            #[test]
            fn creates_directory_when_initialized() -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let wal_path = dir.path().join("test_wal");
                let _wal = IntentWal::open(&wal_path)?;
                assert!(wal_path.exists());
                Ok(())
            }
        }
    }

    mod append {
        use super::*;

        mod with_valid_request {
            use super::*;

            #[test]
            fn persists_intent_to_disk_when_appended() -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let wal = IntentWal::open(dir.path())?;
                let seq = SequenceId::new(1);
                let req = mock_request(1);

                wal.append(seq, &req)?;

                let recovered = wal.recover()?;
                assert_eq!(recovered.len(), 1);
                assert_eq!(recovered[0].0, seq);
                assert_eq!(recovered[0].1.client_id, req.client_id);
                Ok(())
            }
        }
    }

    mod remove {
        use super::*;

        mod address_cleanup {
            use super::*;

            #[test]
            fn removes_intent_idempotently_when_invoked() -> Result<(), Box<dyn std::error::Error>>
            {
                let dir = tempdir()?;
                let wal = IntentWal::open(dir.path())?;
                let seq = SequenceId::new(1);
                wal.append(seq, &mock_request(1))?;

                wal.remove(seq)?;
                assert!(wal.recover()?.is_empty());

                // Second removal should not error
                wal.remove(seq)?;
                Ok(())
            }
        }
    }

    mod recover {
        use super::*;

        mod log_reconstruction {
            use super::*;

            #[test]
            fn maintains_monotonic_ordering_when_recovering_scattered_entries()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let wal = IntentWal::open(dir.path())?;

                // Append out of order
                wal.append(SequenceId::new(10), &mock_request(10))?;
                wal.append(SequenceId::new(5), &mock_request(5))?;
                wal.append(SequenceId::new(15), &mock_request(15))?;

                let recovered = wal.recover()?;
                assert_eq!(recovered.len(), 3);
                assert_eq!(recovered[0].0.as_u64(), 5);
                assert_eq!(recovered[1].0.as_u64(), 10);
                assert_eq!(recovered[2].0.as_u64(), 15);
                Ok(())
            }
        }
    }
}
