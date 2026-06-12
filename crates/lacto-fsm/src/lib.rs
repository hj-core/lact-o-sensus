//! # Lacto-FSM: The Clinical Foundation
//!
//! This module implements the Replicated State Machine (RSM) foundation for the
//! Lact-O-Sensus cluster. It is responsible for the transition from opaque
//! consensus log entries to validated, persistent physical grocery state.
//!
//! ## Core Responsibilities
//!
//! 1. **Persistence (ADR 001/009):** Utilizes `sled` for isolated, tree-based
//!    storage of inventory, session state, and system metadata.
//! 2. **Exactly-Once Semantics (ADR 006):** Enforces linearizability via a
//!    persistent Session Table that deduplicates client intents.
//! 3. **Temporal Determinism (ADR 006):** Maintains a monotonic clinical clock
//!    derived from consensus log timestamps.
//! 4. **Halt Mandate (ADR 009):** Ensures that any invariant violation (e.g.,
//!    sequence gaps, log regression) triggers a controlled transition to the
//!    `Poisoned` state followed by a process panic.
//!
//! ## Tree Architecture
//!
//! - `inventory`: [resolved_item_key (String) => GroceryItem (Protobuf)]
//! - `sessions`: [ClientId (String) => SessionRecord (Protobuf)]
//! - `meta`: [Key (String) => Metadata (Binary)], e.g., last_applied_index.

use std::str::FromStr;

use common::app_api::InventoryReader;
use common::app_api::SessionProvider;
use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::SessionRecord;
use common::raft_api::StateMachine;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::types::errors::FsmError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use prost::Message;
use sled::Db;
use sled::Transactional;
use sled::Tree;
use sled::transaction::TransactionResult;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

mod internal {
    include!(concat!(env!("OUT_DIR"), "/lacto_fsm.v1.rs"));
}

use internal::SnapshotData;

/// Persistent implementation of the Lact-O-Sensus state machine using `sled`.
///
/// This store satisfies the StateMachine trait by deserializing
/// CommittedMutation bytes and updating a localized inventory persisted on
/// disk.
///
/// TREE ARCHITECTURE:
/// To ensure logical isolation within the FSM database:
/// 1. "inventory": Exclusively for [resolved_item_key (String) => GroceryItem
///    (Protobuf)]
/// 2. "sessions": Exclusively for [ClientId (String) => SessionRecord
///    (Protobuf)]
/// 3. "meta": Exclusively for [Key (String) => Metadata (Binary)], e.g.,
///    last_applied_index.
#[derive(Debug)]
pub struct LactoStore {
    db: Db,
    inventory: Tree,
    sessions: Tree,
    meta: Tree,
}

impl LactoStore {
    const KEY_LAST_APPLIED: &'static [u8] = b"last_applied";
    const KEY_LAST_EFFECTIVE_TIME: &'static [u8] = b"last_effective_time";
    const KEY_RESTORE_IN_PROGRESS: &'static [u8] = b"restore_in_progress";
    const TREE_INVENTORY: &'static str = "inventory";
    const TREE_META: &'static str = "meta";
    const TREE_SESSIONS: &'static str = "sessions";

    pub fn new(db: Db) -> Result<Self, FsmError> {
        let inventory = db
            .open_tree(Self::TREE_INVENTORY)
            .map_err(|e| FsmError::persistence(format!("Failed to open inventory tree: {}", e)))?;
        let sessions = db
            .open_tree(Self::TREE_SESSIONS)
            .map_err(|e| FsmError::persistence(format!("Failed to open sessions tree: {}", e)))?;
        let meta = db
            .open_tree(Self::TREE_META)
            .map_err(|e| FsmError::persistence(format!("Failed to open meta tree: {}", e)))?;

        let store = Self {
            db,
            inventory,
            sessions,
            meta,
        };

        // STARTUP SANITIZATION (ADR 011)
        // If a crash happened during snapshot restoration, the dirty flag will
        // be present. We must purge all data to ensure we don't start with a
        // corrupted "Ghost State."
        if store.is_restoration_stale()? {
            warn!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                "HALT RECOVERY: Detected stale restoration attempt. Purging FSM to preserve integrity."
            );
            store.purge_all_data()?;
        }

        Ok(store)
    }

    /// Checks if a restoration flag was left behind from a previous crash.
    fn is_restoration_stale(&self) -> Result<bool, FsmError> {
        let flag = self
            .meta
            .get(Self::KEY_RESTORE_IN_PROGRESS)
            .map_err(|e| FsmError::persistence(format!("Failed to read restore flag: {}", e)))?
            .map(|v| v.as_ref() == b"true")
            .unwrap_or(false);
        Ok(flag)
    }

    /// Hard wipe of all FSM data trees and metadata.
    fn purge_all_data(&self) -> Result<(), FsmError> {
        self.inventory
            .clear()
            .map_err(|e| FsmError::persistence(format!("Failed to purge inventory: {}", e)))?;
        self.sessions
            .clear()
            .map_err(|e| FsmError::persistence(format!("Failed to purge sessions: {}", e)))?;
        self.meta
            .clear()
            .map_err(|e| FsmError::persistence(format!("Failed to purge meta: {}", e)))?;
        self.db
            .flush()
            .map_err(|e| FsmError::persistence(format!("Failed to flush after purge: {}", e)))?;
        Ok(())
    }

    /// Retrieves the system-wide clinical time derived from the consensus log.
    pub fn last_effective_time(&self) -> Result<prost_types::Timestamp, FsmError> {
        self.meta
            .get(Self::KEY_LAST_EFFECTIVE_TIME)
            .map_err(|e| FsmError::persistence(format!("Failed to read meta tree: {}", e)))?
            .map(|bytes| Self::decode_timestamp(bytes.as_ref()))
            .transpose()
            .map(|opt| {
                opt.unwrap_or(prost_types::Timestamp {
                    seconds: 0,
                    nanos: 0,
                })
            })
    }

    /// Factory to construct a GroceryItem from a committed mutation record.
    fn item_from_mutation(index: LogIndex, mutation: CommittedMutation) -> GroceryItem {
        GroceryItem::new(
            mutation.resolved_item_key,
            mutation.updated_base_quantity,
            mutation.base_unit,
            mutation.updated_category,
            mutation.client_id,
            mutation.event_time.unwrap_or_default(),
            index,
            mutation.display_unit,
        )
    }

    /// Internal helper to retrieve and decode a SessionRecord from sled.
    fn get_session_record(&self, client_id: &str) -> Result<Option<SessionRecord>, FsmError> {
        self.sessions
            .get(client_id.as_bytes())
            .map_err(|e| FsmError::persistence(format!("Failed to read session table: {}", e)))?
            .map(|bytes| {
                SessionRecord::decode(bytes.as_ref()).map_err(|e| {
                    FsmError::deserialization(format!(
                        "Corrupt SessionRecord for client {}: {}",
                        client_id, e
                    ))
                })
            })
            .transpose()
    }

    /// Internal helper to decode a prost Timestamp from raw sled bytes.
    /// Returns an error on deserialization failure to trigger the Halt Mandate.
    fn decode_timestamp(bytes: &[u8]) -> Result<prost_types::Timestamp, FsmError> {
        prost_types::Timestamp::decode(bytes).map_err(|e| {
            FsmError::deserialization(format!("Failed to decode clinical timestamp: {}", e))
        })
    }

    /// Returns the later of two clinical timestamps to ensure temporal
    /// monotonicity.
    fn max_timestamp(
        t1: prost_types::Timestamp,
        t2: prost_types::Timestamp,
    ) -> prost_types::Timestamp {
        if t1.seconds > t2.seconds || (t1.seconds == t2.seconds && t1.nanos > t2.nanos) {
            t1
        } else {
            t2
        }
    }

    /// Internal helper to decode a raw sled result into a GroceryItem.
    /// Performs boundary validation and logs any physical corruption.
    fn decode_inventory_entry(res: sled::Result<(sled::IVec, sled::IVec)>) -> Option<GroceryItem> {
        match res {
            Ok((_, v)) => match GroceryItem::decode(v.as_ref()) {
                Ok(item) => Some(item),
                Err(e) => {
                    error!(
                        target: ClinicalTarget::ClinicalFsm.as_str(),
                        error = %e,
                        "Skipping corrupt inventory record"
                    );
                    None
                }
            },
            Err(e) => {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    error = %e,
                    "Database error during inventory iteration"
                );
                None
            }
        }
    }
}

impl SessionProvider for LactoStore {
    fn check_session(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
    ) -> Result<Option<SessionRecord>, FsmError> {
        match self.get_session_record(client_id.as_str())? {
            Some(record)
                if sequence_id.as_u64() == 0 || record.last_sequence_id == sequence_id.as_u64() =>
            {
                Ok(Some(record))
            }
            _ => Ok(None),
        }
    }
}

impl InventoryReader for LactoStore {
    fn get_inventory(&self) -> Vec<GroceryItem> {
        self.inventory
            .iter()
            .filter_map(Self::decode_inventory_entry)
            .collect()
    }

    fn current_version(&self) -> Result<LogIndex, FsmError> {
        StateMachine::last_applied_index(self)
    }
}

impl StateMachine for LactoStore {
    type Error = FsmError;

    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
        self.meta
            .get(Self::KEY_LAST_APPLIED)
            .map_err(|e| {
                FsmError::persistence(format!("Failed to read last_applied index: {}", e))
            })?
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    FsmError::deserialization("last_applied index byte conversion failed")
                })?;
                Ok(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(LogIndex::ZERO))
    }

    #[tracing::instrument(
        target = "clinical::fsm",
        skip(self, data),
        fields(
            index = %index,
            client_id = tracing::field::Empty,
            seq = tracing::field::Empty,
        )
    )]
    fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Self::Error> {
        let mutation = CommittedMutation::decode(data).map_err(|e| {
            FsmError::deserialization(format!(
                "Failed to deserialize mutation at index {}: {}",
                index, e
            ))
        })?;

        // 1. Physical Log Monotonicity (Physical Fence)
        let current_applied = self.last_applied_index()?;
        if index != (current_applied + 1)? {
            error!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                last_applied = %current_applied,
                got = %index,
                "HALT MANDATE (ADR 009): Non-sequential LogIndex apply attempted. Log regression or divergence suspected."
            );
            return Err(FsmError::invariant(format!(
                "Non-sequential LogIndex apply attempted. last_applied={}, got {}",
                current_applied, index
            )));
        }

        let client_id = mutation.client_id.clone();
        let seq = SequenceId::new(mutation.sequence_id);

        let client_id_obj = match ClientId::from_str(&client_id) {
            Ok(id) => id,
            Err(e) => {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    raw_id = %client_id,
                    error = %e,
                    "HALT MANDATE (ADR 004): Invalid client_id in ledger. Identity metadata is corrupted."
                );
                return Err(FsmError::invariant(format!(
                    "Invalid client_id '{}' in ledger at index {}. Identity metadata is \
                     corrupted: {}",
                    client_id, index, e
                )));
            }
        };

        // Record truncated client_id in the span context
        tracing::Span::current().record("client_id", client_id_obj.truncated());
        tracing::Span::current().record("seq", seq.as_u64());

        // 2. Client Sequence Validation (ADR 006)
        let last_seen = SequenceId::new(
            self.get_session_record(&client_id)?
                .map(|r| r.last_sequence_id)
                .unwrap_or(0),
        );
        let expected_seq = (last_seen + 1)?;

        if seq <= last_seen {
            warn!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                last_seen = %last_seen,
                "Deduplicating stale/retry sequence. State advancement only."
            );
            // Even for duplicates, we must advance last_applied to ensure
            // the Raft engine stays in sync with the log.
            self.meta
                .insert(Self::KEY_LAST_APPLIED, &index.as_u64().to_be_bytes())
                .map_err(|e| {
                    FsmError::persistence(format!("Failed to persist last_applied index: {}", e))
                })?;
            self.db.flush().map_err(|e| {
                FsmError::persistence(format!("FSM flush failure during deduplication: {}", e))
            })?;
            return Ok(());
        }

        if seq > expected_seq {
            // ADR 006: Invariant Enforcement
            error!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                expected_seq = %expected_seq,
                "HALT MANDATE (ADR 006): Sequence gap detected. Causal history broken."
            );
            return Err(FsmError::invariant(format!(
                "Sequence gap for client {}: expected {}, got {}",
                client_id, expected_seq, seq
            )));
        }

        // --- Phase 2: Atomic Commitment (Inventory + Session + Index + Time) ---
        let inventory_tree = self.inventory.clone();
        let sessions_tree = self.sessions.clone();
        let meta_tree = self.meta.clone();

        let status = match MutationStatus::try_from(mutation.status) {
            Ok(s) => s,
            Err(_) => {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    status_int = %mutation.status,
                    "HALT MANDATE (ADR 009): Unknown MutationStatus integer. Protocol version mismatch or ledger corruption."
                );
                return Err(FsmError::invariant(format!(
                    "Unknown MutationStatus integer {} at index {}. The node is likely running an \
                     obsolete version or the ledger is corrupted.",
                    mutation.status, index
                )));
            }
        };
        let moral_justification = mutation.moral_justification.clone();

        // Stateful Temporal Determinism (ADR 006)
        let event_time = match mutation.event_time {
            Some(t) => t,
            None => {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    "HALT MANDATE (ADR 006): Mutation is missing mandatory event_time. Cannot update deterministic clinical clock."
                );
                return Err(FsmError::invariant(format!(
                    "Mutation at index {} is missing mandatory event_time. Cannot update \
                     deterministic clinical clock.",
                    index
                )));
            }
        };

        // Global deterministic time update logic
        let current_effective = self.last_effective_time()?;
        let next_effective = Self::max_timestamp(event_time, current_effective);

        // PII Trace: Log full AI output and raw intent at TRACE level only (ADR 010).
        trace!(
            target: ClinicalTarget::ClinicalFsm.as_str(),
            raw_client_id = %client_id,
            item_key = %mutation.resolved_item_key,
            justification = %moral_justification,
            raw_input = %mutation.raw_user_input,
            "Physical Mutation PII Trace"
        );

        let res: TransactionResult<(), ()> = (&inventory_tree, &sessions_tree, &meta_tree)
            .transaction(|(inventory, sessions, meta)| {
                // 1. Update Physical Inventory (only if Committed)
                if status == MutationStatus::Committed {
                    if mutation.is_delete {
                        inventory.remove(mutation.resolved_item_key.as_bytes())?;
                    } else {
                        let item = Self::item_from_mutation(index, mutation.clone());
                        inventory.insert(
                            mutation.resolved_item_key.as_bytes(),
                            item.encode_to_vec().as_slice(),
                        )?;
                    }
                }

                // 2. Update Session Table (ADR 006)
                let record = SessionRecord::new(
                    &client_id_obj,
                    seq,
                    status,
                    index,
                    moral_justification.clone(),
                    event_time,
                );
                sessions.insert(client_id.as_bytes(), record.encode_to_vec().as_slice())?;

                // 3. Update Apply Index & Clinical Time
                meta.insert(Self::KEY_LAST_APPLIED, &index.as_u64().to_be_bytes())?;
                meta.insert(
                    Self::KEY_LAST_EFFECTIVE_TIME,
                    next_effective.encode_to_vec().as_slice(),
                )?;

                Ok(())
            });

        res.map_err(|e| FsmError::persistence(format!("Atomic transaction failure: {:?}", e)))?;

        // Synchronous flush as mandated by ADR 001
        self.db.flush().map_err(|e| {
            FsmError::persistence(format!("FSM flush failure after commitment: {}", e))
        })?;

        info!(
            target: ClinicalTarget::ClinicalFsm.as_str(),
            status = ?status,
            clinical_time = %format!("{}.{:09}s", next_effective.seconds, next_effective.nanos),
            "Mutation applied to state machine."
        );

        Ok(())
    }

    /// Serializes the entire state machine into a contiguous byte vector.
    ///
    /// CONSISTENCY MANDATE (ADR 011):
    /// This method performs multiple independent tree iterations. Consistency
    /// is guaranteed by the Raft orchestrator, which MUST pause the `apply`
    /// pipeline (Freeze-Apply) before calling this method, ensuring no
    /// mutations occur during serialization.
    #[tracing::instrument(name = "fsm_snapshot", target = "raft::compaction", skip_all)]
    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
        let inventory = self.get_inventory();

        let mut sessions = Vec::new();
        for res in self.sessions.iter() {
            let (_, v) = res
                .map_err(|e| FsmError::persistence(format!("Session iteration failure: {}", e)))?;
            sessions.push(SessionRecord::decode(v.as_ref()).map_err(|e| {
                FsmError::deserialization(format!("Failed to decode SessionRecord: {}", e))
            })?);
        }

        let last_effective_time = Some(self.last_effective_time()?);

        let data = SnapshotData {
            inventory,
            sessions,
            last_effective_time,
        };

        Ok(data.encode_to_vec())
    }

    /// Destructively restores the state machine from a snapshot.
    ///
    /// HALT MANDATE (ADR 009):
    /// Any failure during restoration (deserialization or physical I/O)
    /// indicates a corrupted node state and MUST return an error that
    /// triggers a panic.
    ///
    /// CRASH RECOVERY (ADR 011):
    /// Utilizes the "Restoration Tombstone" protocol to ensure atomicity
    /// across multiple trees without full-DB transactions.
    #[tracing::instrument(
        name = "fsm_install_snapshot",
        target = "raft::compaction",
        skip_all,
        fields(index = %last_included_index, trace_id = %trace_id)
    )]
    fn install_snapshot(
        &self,
        last_included_index: LogIndex,
        data: &[u8],
        trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        let snapshot = SnapshotData::decode(data).map_err(|e| {
            // HALT FORENSICS (Rule 15, ADR 010)
            error!(
                target: ClinicalTarget::ClinicalTelemetry.as_str(),
                index = %last_included_index,
                trace_id = %trace_id,
                error = %e,
                "HALT MANDATE (ADR 009/011): Snapshot deserialization failed. Physical integrity compromised."
            );
            FsmError::deserialization(format!("Failed to decode SnapshotData: {}", e))
        })?;

        // 1. Mark as Dirty (The Tombstone)
        self.meta
            .insert(Self::KEY_RESTORE_IN_PROGRESS, b"true")
            .map_err(|e| FsmError::persistence(format!("Failed to set restore flag: {}", e)))?;
        self.db.flush().map_err(|e| {
            FsmError::persistence(format!("FSM flush failure during dirty-marking: {}", e))
        })?;

        // 2. Clear existing state
        self.inventory
            .clear()
            .map_err(|e| FsmError::persistence(format!("Failed to clear inventory: {}", e)))?;
        self.sessions
            .clear()
            .map_err(|e| FsmError::persistence(format!("Failed to clear sessions: {}", e)))?;
        // We clear everything EXCEPT the dirty flag in meta
        for res in self.meta.iter() {
            let (k, _) = res.map_err(|e| {
                FsmError::persistence(format!("Meta iteration failure during clear: {}", e))
            })?;
            if k.as_ref() != Self::KEY_RESTORE_IN_PROGRESS {
                self.meta.remove(k).map_err(|e| {
                    FsmError::persistence(format!("Failed to remove meta key: {}", e))
                })?;
            }
        }

        // 3. Restore Inventory in Batch
        let mut inv_batch = sled::Batch::default();
        for item in &snapshot.inventory {
            inv_batch.insert(item.item_key.as_bytes(), item.encode_to_vec());
        }
        self.inventory.apply_batch(inv_batch).map_err(|e| {
            FsmError::persistence(format!("Failed to apply inventory batch: {}", e))
        })?;

        // 4. Restore Sessions in Batch
        let mut sess_batch = sled::Batch::default();
        for record in &snapshot.sessions {
            sess_batch.insert(record.client_id.as_bytes(), record.encode_to_vec());
        }
        self.sessions
            .apply_batch(sess_batch)
            .map_err(|e| FsmError::persistence(format!("Failed to apply session batch: {}", e)))?;

        // 5. Finalize Metadata and Mark as Clean
        let mut meta_batch = sled::Batch::default();
        meta_batch.insert(
            Self::KEY_LAST_APPLIED,
            &last_included_index.as_u64().to_be_bytes(),
        );

        if let Some(time) = snapshot.last_effective_time {
            meta_batch.insert(Self::KEY_LAST_EFFECTIVE_TIME, time.encode_to_vec());
        }

        // Remove the Tombstone
        meta_batch.remove(Self::KEY_RESTORE_IN_PROGRESS);

        self.meta
            .apply_batch(meta_batch)
            .map_err(|e| FsmError::persistence(format!("Failed to apply meta batch: {}", e)))?;

        // 6. Synchronous flush (ADR 001)
        self.db.flush().map_err(|e| {
            FsmError::persistence(format!(
                "FSM flush failure after snapshot restoration: {}",
                e
            ))
        })?;

        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            index = %last_included_index,
            inventory_count = snapshot.inventory.len(),
            session_count = snapshot.sessions.len(),
            "State machine snapshot installed successfully."
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::proto::v1::app::MutationStatus;
    use common::types::ClientId;
    use common::types::SequenceId;
    use tempfile::tempdir;

    use super::*;

    fn setup_store() -> LactoStore {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        LactoStore::new(db).unwrap()
    }

    /// Flexible mock helper to support various EOS scenarios.
    fn mock_mutation(cid: &ClientId, seq: u64, status: MutationStatus) -> CommittedMutation {
        CommittedMutation::new(
            cid,
            SequenceId::new(seq),
            format!("milk-{}", cid), // Qualified for isolation
            "Milk".to_string(),
            "1000".to_string(),
            "ml".to_string(),
            "ml".to_string(),
            "Dairy".to_string(),
            "add milk".to_string(),
            "Approved".to_string(),
            false,
            status,
            std::time::SystemTime::now(),
        )
    }

    mod apply {
        use super::*;

        mod physical_log_continuity {
            use super::*;
            #[test]
            fn returns_invariant_error_when_log_index_skips_ahead() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut data)
                    .unwrap();

                // 1. Success: Apply Index 1
                store.apply(LogIndex::new(1), &data).unwrap();

                // 2. Failure: Try to apply Index 3 (skipping Index 2)
                let result = store.apply(LogIndex::new(3), &data);
                assert!(
                    matches!(result, Err(FsmError::Invariant(ref msg)) if msg.contains("Non-sequential")),
                    "Expected Invariant error for non-sequential apply, got {:?}",
                    result
                );
            }
        }

        mod exactly_once_semantics {
            use super::*;

            #[test]
            fn advances_last_applied_when_sequence_is_duplicate() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut data)
                    .unwrap();

                store.apply(LogIndex::new(1), &data).unwrap();
                store.apply(LogIndex::new(2), &data).unwrap();

                assert_eq!(store.last_applied_index().unwrap(), LogIndex::new(2));
                assert_eq!(store.get_inventory().len(), 1);
            }

            #[test]
            fn advances_last_applied_when_sequence_is_stale() {
                let store = setup_store();
                let cid = ClientId::generate();

                let mut data1 = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut data1)
                    .unwrap();
                let mut data2 = Vec::new();
                mock_mutation(&cid, 2, MutationStatus::Committed)
                    .encode(&mut data2)
                    .unwrap();

                store.apply(LogIndex::new(1), &data1).unwrap();
                store.apply(LogIndex::new(2), &data2).unwrap();

                // Late arrival of seq 1 at Index 3
                store.apply(LogIndex::new(3), &data1).unwrap();
                assert_eq!(store.last_applied_index().unwrap(), LogIndex::new(3));
            }

            #[test]
            fn returns_invariant_error_when_sequence_gap_detected() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                mock_mutation(&cid, 2, MutationStatus::Committed)
                    .encode(&mut data)
                    .unwrap();

                let result = store.apply(LogIndex::new(1), &data);
                assert!(
                    matches!(result, Err(FsmError::Invariant(ref msg)) if msg.contains("Sequence gap")),
                    "Expected Invariant error for sequence gap, got {:?}",
                    result
                );
            }

            #[test]
            fn maintains_independent_sequences_when_multiple_clients_active() {
                let store = setup_store();
                let cid1 = ClientId::generate();
                let cid2 = ClientId::generate();

                let mut d1 = Vec::new();
                mock_mutation(&cid1, 1, MutationStatus::Committed)
                    .encode(&mut d1)
                    .unwrap();
                let mut d2 = Vec::new();
                mock_mutation(&cid2, 1, MutationStatus::Committed)
                    .encode(&mut d2)
                    .unwrap();

                store.apply(LogIndex::new(1), &d1).unwrap();
                store.apply(LogIndex::new(2), &d2).unwrap();

                assert_eq!(store.get_inventory().len(), 2);
            }
        }

        mod inventory_mutations {
            use super::*;

            #[test]
            fn updates_inventory_when_status_is_committed() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut data)
                    .unwrap();

                store.apply(LogIndex::new(1), &data).unwrap();
                assert_eq!(store.get_inventory().len(), 1);
            }

            #[test]
            fn does_not_update_inventory_when_status_is_vetoed() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Vetoed)
                    .encode(&mut data)
                    .unwrap();

                store.apply(LogIndex::new(1), &data).unwrap();
                assert!(store.get_inventory().is_empty());
            }

            #[test]
            fn deletes_item_when_is_delete_is_true() {
                let store = setup_store();
                let cid = ClientId::generate();

                let mut d1 = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut d1)
                    .unwrap();
                store.apply(LogIndex::new(1), &d1).unwrap();

                let mut d2 = Vec::new();
                let mut m2 = mock_mutation(&cid, 2, MutationStatus::Committed);
                m2.is_delete = true;
                m2.encode(&mut d2).unwrap();

                store.apply(LogIndex::new(2), &d2).unwrap();
                assert!(store.get_inventory().is_empty());
            }
        }

        mod persistence {
            use super::*;
            #[test]
            fn recovers_consistent_state_when_restarted_from_disk() {
                let dir = tempdir().unwrap();
                let db_path = dir.path();
                let cid = ClientId::generate();
                {
                    let db = sled::open(db_path).unwrap();
                    let store = LactoStore::new(db).unwrap();
                    let mut data = Vec::new();
                    mock_mutation(&cid, 1, MutationStatus::Committed)
                        .encode(&mut data)
                        .unwrap();
                    store.apply(LogIndex::new(1), &data).unwrap();
                }
                {
                    let db = sled::open(db_path).unwrap();
                    let store = LactoStore::new(db).unwrap();
                    assert_eq!(store.last_applied_index().unwrap(), LogIndex::new(1));
                    assert_eq!(store.get_inventory().len(), 1);
                }
            }
        }

        mod safety_mandates {
            use super::*;

            #[test]
            fn returns_error_on_corrupt_mutation_bytes() {
                let store = setup_store();
                let data = vec![0xFF, 0xFF, 0xFF]; // Invalid protobuf bytes

                let result = store.apply(LogIndex::new(1), &data);
                assert!(
                    matches!(result, Err(FsmError::Deserialization(ref msg)) if msg.contains("deserialize mutation")),
                    "Expected Deserialization error, got {:?}",
                    result
                );
            }

            #[test]
            fn returns_error_when_mutation_missing_event_time() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut mutation = mock_mutation(&cid, 1, MutationStatus::Committed);
                mutation.event_time = None; // Explicitly violate the mandate

                let mut data = Vec::new();
                mutation.encode(&mut data).unwrap();

                let result = store.apply(LogIndex::new(1), &data);
                assert!(
                    matches!(result, Err(FsmError::Invariant(ref msg)) if msg.contains("mandatory event_time")),
                    "Expected Invariant error for missing event_time, got {:?}",
                    result
                );
            }

            #[test]
            fn returns_invariant_error_on_unknown_mutation_status() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut mutation = mock_mutation(&cid, 1, MutationStatus::Committed);
                mutation.status = 99; // Unknown status code

                let mut data = Vec::new();
                mutation.encode(&mut data).unwrap();

                let result = store.apply(LogIndex::new(1), &data);
                assert!(
                    matches!(result, Err(FsmError::Invariant(ref msg)) if msg.contains("Unknown MutationStatus")),
                    "Expected Invariant error for unknown status, got {:?}",
                    result
                );
            }

            #[test]
            fn returns_invariant_error_on_invalid_client_id() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut mutation = mock_mutation(&cid, 1, MutationStatus::Committed);
                mutation.client_id = "not-a-uuid".to_string(); // Invalid ID format

                let mut data = Vec::new();
                mutation.encode(&mut data).unwrap();

                let result = store.apply(LogIndex::new(1), &data);
                assert!(
                    matches!(result, Err(FsmError::Invariant(ref msg)) if msg.contains("Invalid client_id")),
                    "Expected Invariant error for invalid client_id, got {:?}",
                    result
                );
            }
        }

        mod temporal_determinism {
            use prost_types::Timestamp;

            use super::*;

            #[test]
            fn advances_global_clinical_time_monotonically() {
                let store = setup_store();
                let cid = ClientId::generate();

                // 1. Apply mutation at T=100
                let mut m1 = mock_mutation(&cid, 1, MutationStatus::Committed);
                m1.event_time = Some(Timestamp {
                    seconds: 100,
                    nanos: 0,
                });
                let mut d1 = Vec::new();
                m1.encode(&mut d1).unwrap();
                store.apply(LogIndex::new(1), &d1).unwrap();
                assert_eq!(store.last_effective_time().unwrap().seconds, 100);

                // 2. Apply mutation at T=200
                let mut m2 = mock_mutation(&cid, 2, MutationStatus::Committed);
                m2.event_time = Some(Timestamp {
                    seconds: 200,
                    nanos: 0,
                });
                let mut d2 = Vec::new();
                m2.encode(&mut d2).unwrap();
                store.apply(LogIndex::new(2), &d2).unwrap();
                assert_eq!(store.last_effective_time().unwrap().seconds, 200);

                // 3. Apply mutation at T=150 (Stale Log Clock)
                // Clinical time should NOT regress
                let mut m3 = mock_mutation(&cid, 3, MutationStatus::Committed);
                m3.event_time = Some(Timestamp {
                    seconds: 150,
                    nanos: 0,
                });
                let mut d3 = Vec::new();
                m3.encode(&mut d3).unwrap();
                store.apply(LogIndex::new(3), &d3).unwrap();
                assert_eq!(store.last_effective_time().unwrap().seconds, 200);
            }

            #[test]
            fn persists_per_session_activity_timestamp() {
                let store = setup_store();
                let cid = ClientId::generate();
                let event_time = Timestamp {
                    seconds: 12345,
                    nanos: 678,
                };

                let mut mutation = mock_mutation(&cid, 1, MutationStatus::Committed);
                mutation.event_time = Some(event_time);
                let mut data = Vec::new();
                mutation.encode(&mut data).unwrap();

                store.apply(LogIndex::new(1), &data).unwrap();

                let record = store
                    .check_session(&cid, SequenceId::new(1))
                    .unwrap()
                    .unwrap();

                assert_eq!(record.last_activity_effective_time, Some(event_time));
            }
        }
    }

    mod check_session {
        use super::*;

        mod wildcard_queries {
            use super::*;
            #[test]
            fn returns_latest_record_when_sequence_id_is_zero() {
                let store = setup_store();
                let cid = ClientId::generate();

                // 1. Apply sequence 1
                let mut data1 = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut data1)
                    .unwrap();
                store.apply(LogIndex::new(1), &data1).unwrap();

                // 2. Apply sequence 2
                let mut data2 = Vec::new();
                mock_mutation(&cid, 2, MutationStatus::Committed)
                    .encode(&mut data2)
                    .unwrap();
                store.apply(LogIndex::new(2), &data2).unwrap();

                // 3. Query with seq 0 (wildcard) returns the latest (2)
                let record = store
                    .check_session(&cid, SequenceId::new(0))
                    .unwrap()
                    .unwrap();
                assert_eq!(record.last_sequence_id, 2);
            }
        }

        mod session_lookup {
            use super::*;
            #[test]
            fn returns_none_when_session_does_not_exist() {
                let store = setup_store();
                let result = store
                    .check_session(&ClientId::generate(), SequenceId::new(1))
                    .unwrap();
                assert!(result.is_none());
            }

            #[test]
            fn returns_accurate_replay_data_when_session_retrieved() {
                let store = setup_store();
                let cid = ClientId::generate();
                let mut data = Vec::new();
                let mut m = mock_mutation(&cid, 1, MutationStatus::Vetoed);
                m.moral_justification = "Vetoed justification".to_string();
                m.encode(&mut data).unwrap();

                store.apply(LogIndex::new(1), &data).unwrap();
                let record = store
                    .check_session(&cid, SequenceId::new(1))
                    .unwrap()
                    .unwrap();
                assert_eq!(record.moral_justification, "Vetoed justification");
            }
        }
    }

    mod snapshot_and_restoration {
        use super::*;

        mod round_trip {
            use super::*;

            #[test]
            fn restores_identical_state_from_snapshot() {
                let store_a = setup_store();
                let cid = ClientId::generate();

                // 1. Populate Store A
                let mut d1 = Vec::new();
                mock_mutation(&cid, 1, MutationStatus::Committed)
                    .encode(&mut d1)
                    .unwrap();
                store_a.apply(LogIndex::new(1), &d1).unwrap();

                let mut d2 = Vec::new();
                let mut m2 = mock_mutation(&cid, 2, MutationStatus::Committed);
                m2.resolved_item_key = "bread".to_string();
                m2.encode(&mut d2).unwrap();
                store_a.apply(LogIndex::new(2), &d2).unwrap();

                // 2. Take Snapshot
                let snapshot_bytes = store_a.snapshot().unwrap();
                let last_index = store_a.last_applied_index().unwrap();

                // 3. Restore into Store B
                let store_b = setup_store();
                store_b
                    .install_snapshot(last_index, &snapshot_bytes, TraceId::generate())
                    .unwrap();

                // 4. Verify Equality
                assert_eq!(store_b.last_applied_index().unwrap(), last_index);
                assert_eq!(store_b.get_inventory().len(), 2);

                let session = store_b
                    .check_session(&cid, SequenceId::new(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(session.last_sequence_id, 2);

                assert_eq!(
                    store_b.last_effective_time().unwrap().seconds,
                    store_a.last_effective_time().unwrap().seconds
                );
            }
        }

        mod crash_recovery {
            use super::*;

            #[test]
            fn purges_dirty_state_on_startup_if_tombstone_present() {
                let dir = tempfile::tempdir().unwrap();
                let db_path = dir.path();

                // 1. Create a "Dirty" state manually
                {
                    let db = sled::open(db_path).unwrap();
                    let store = LactoStore::new(db).unwrap();

                    // Add some "Ghost" data
                    let mut data = Vec::new();
                    mock_mutation(&ClientId::generate(), 1, MutationStatus::Committed)
                        .encode(&mut data)
                        .unwrap();
                    store.apply(LogIndex::new(1), &data).unwrap();

                    // Manually insert the Tombstone
                    store
                        .meta
                        .insert(LactoStore::KEY_RESTORE_IN_PROGRESS, b"true")
                        .unwrap();
                    store.db.flush().unwrap();
                }

                // 2. Initialize new LactoStore (Simulation of Reboot)
                {
                    let db = sled::open(db_path).unwrap();
                    let store = LactoStore::new(db).unwrap();

                    // 3. Verify Sanitization
                    assert_eq!(store.last_applied_index().unwrap(), LogIndex::new(0));
                    assert!(store.get_inventory().is_empty());
                    assert!(!store.is_restoration_stale().unwrap());
                }
            }
        }

        mod install_snapshot {
            use std::sync::Arc;
            use std::thread;
            use std::time::Duration;

            use super::*;

            #[test]
            fn tombstone_lifecycle() {
                // Prepare snapshot data from Store A
                let store_a = setup_store();
                let cid = ClientId::generate();
                for i in 1..=50 {
                    let mut data = Vec::new();
                    let mut m = mock_mutation(&cid, i, MutationStatus::Committed);
                    m.resolved_item_key = format!("item-{}", i);
                    m.encode(&mut data).unwrap();
                    store_a.apply(LogIndex::new(i), &data).unwrap();
                }
                let snapshot_data = store_a.snapshot().unwrap();

                // Store B: will run install_snapshot in a background thread
                let store_b = Arc::new(setup_store());

                // Verify no tombstone before install_snapshot
                assert!(
                    !store_b
                        .meta
                        .contains_key(LactoStore::KEY_RESTORE_IN_PROGRESS)
                        .unwrap()
                );

                let store_clone = store_b.clone();
                let handle = thread::spawn(move || {
                    store_clone
                        .install_snapshot(LogIndex::new(100), &snapshot_data, TraceId::generate())
                        .unwrap();
                });

                // Poll for tombstone presence during install_snapshot execution.
                // install_snapshot writes the tombstone in step 1 and clears it in
                // step 5. Since steps 2-4 involve I/O with 50 items, the tombstone
                // window is long enough for reliable observation.
                let tombstone_detected = loop {
                    if store_b
                        .meta
                        .contains_key(LactoStore::KEY_RESTORE_IN_PROGRESS)
                        .unwrap()
                    {
                        break true;
                    }
                    if handle.is_finished() {
                        break false;
                    }
                    thread::sleep(Duration::from_micros(500));
                };

                assert!(
                    tombstone_detected,
                    "Tombstone flag must be observable during install_snapshot execution"
                );

                handle.join().unwrap();

                // After completion, tombstone must be cleared
                assert!(
                    !store_b
                        .meta
                        .contains_key(LactoStore::KEY_RESTORE_IN_PROGRESS)
                        .unwrap()
                );
                assert_eq!(store_b.last_applied_index().unwrap(), LogIndex::new(100));
            }
        }
    }
}
