use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use common::proto::v1::app::ProposeMutationRequest;
use common::types::SequenceId;
use prost::Message;
use sled::Db;

/// A Write-Ahead Log (WAL) for ensuring the durability of mutation intents.
///
/// In accordance with ADR 001 (Crash-Recovery), the `IntentWal` persists
/// intents to disk before they are dispatched over the network. This ensures
/// that if the client crashes before receiving a confirmation, the intents
/// can be re-proposed upon recovery, maintaining Exactly-Once Semantics (EOS).
pub struct IntentWal {
    db: Db,
}

impl IntentWal {
    /// Opens the WAL at the specified path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = sled::open(path).context("Failed to open sled WAL database")?;
        Ok(Self { db })
    }

    /// Appends a new mutation intent to the WAL.
    ///
    /// This must be called BEFORE the RPC is dispatched to the cluster.
    pub fn append(&self, sequence_id: SequenceId, req: &ProposeMutationRequest) -> Result<()> {
        let key = sequence_id.value().to_be_bytes();
        let value = req.encode_to_vec();

        self.db
            .insert(key, value)
            .context("Failed to insert intent into WAL")?;

        // Mandatory fsync before acknowledging persistence (ADR 001).
        self.db
            .flush()
            .context("Failed to flush WAL to stable storage")?;
        Ok(())
    }

    /// Removes an intent from the WAL.
    ///
    /// This should be called once the mutation has reached a terminal state
    /// (e.g., COMMITTED or VETOED).
    pub fn remove(&self, sequence_id: SequenceId) -> Result<()> {
        let key = sequence_id.value().to_be_bytes();

        self.db
            .remove(key)
            .context("Failed to remove intent from WAL")?;

        self.db
            .flush()
            .context("Failed to flush WAL after removal")?;
        Ok(())
    }

    /// Recovers all pending intents from the WAL.
    ///
    /// Returns intents sorted by sequence ID to maintain absolute temporal
    /// ordering during recovery.
    pub fn recover(&self) -> Result<Vec<(SequenceId, ProposeMutationRequest)>> {
        let mut recovered = Vec::new();

        for item in self.db.iter() {
            let (key, value) = item.context("Failed to iterate WAL entries")?;
            let entry = self.decode_entry(key, value)?;
            recovered.push(entry);
        }

        // Ensure monotonic recovery order.
        recovered.sort_by_key(|(seq, _)| seq.value());
        Ok(recovered)
    }

    /// Internal helper to decode a WAL entry from raw bytes.
    fn decode_entry(
        &self,
        key: sled::IVec,
        value: sled::IVec,
    ) -> Result<(SequenceId, ProposeMutationRequest)> {
        let seq_val = u64::from_be_bytes(
            key.as_ref()
                .try_into()
                .map_err(|_| anyhow::anyhow!("WAL corruption: invalid sequence ID key length"))?,
        );

        let req = ProposeMutationRequest::decode(value.as_ref())
            .context("WAL corruption: failed to decode mutation request")?;

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
            MutationIntent {
                item_key: "milk".to_string(),
                quantity: Some("1".to_string()),
                unit: None,
                category: None,
                operation: OperationType::Add as i32,
            },
        )
    }

    mod open {
        use super::*;

        #[test]
        fn creates_directory_on_init() -> Result<()> {
            let dir = tempdir()?;
            let wal_path = dir.path().join("test_wal");
            let _wal = IntentWal::open(&wal_path)?;
            assert!(wal_path.exists());
            Ok(())
        }
    }

    mod append {
        use super::*;

        #[test]
        fn persists_intent_to_disk() -> Result<()> {
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

    mod remove {
        use super::*;

        #[test]
        fn removes_intent_idempotently() -> Result<()> {
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

    mod recover {
        use super::*;

        #[test]
        fn maintains_monotonic_ordering() -> Result<()> {
            let dir = tempdir()?;
            let wal = IntentWal::open(dir.path())?;

            // Append out of order
            wal.append(SequenceId::new(10), &mock_request(10))?;
            wal.append(SequenceId::new(5), &mock_request(5))?;
            wal.append(SequenceId::new(15), &mock_request(15))?;

            let recovered = wal.recover()?;
            assert_eq!(recovered.len(), 3);
            assert_eq!(recovered[0].0.value(), 5);
            assert_eq!(recovered[1].0.value(), 10);
            assert_eq!(recovered[2].0.value(), 15);
            Ok(())
        }
    }
}
