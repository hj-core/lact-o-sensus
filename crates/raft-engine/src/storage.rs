//! Physical storage abstraction and persistent implementations for Raft.
//!
//! This module provides the foundational persistence layer for the Raft
//! consensus engine. It adheres to the "Tri-Layer Onion" model (ADR 009) by
//! isolating physical disk I/O from logical consensus orchestration.
//!
//! The primary implementation, `SledStorage`, utilizes the `sled` embedded
//! database to provide atomic, crash-recovery-safe (ADR 001) storage for:
//! 1. **Hard State**: Persistent term and voting records (§5.1).
//! 2. **Log Entries**: Replicated consensus commands (§5.3).
//! 3. **Commit Index**: The highest known committed log entry.

use std::cmp;
use std::fmt::Debug;

use common::proto::v1::raft::HardState;
use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::LogStorageError;
use common::types::trace::ClinicalTarget;
use prost::Message;
use tracing::debug;
use tracing::instrument;

/// Physical storage abstraction for Raft consensus state (§5.1, §5.3).
///
/// Implementations are responsible for persisting the Hard State (Term, Vote)
/// and the Log entries to stable storage, ensuring crash-recovery mandates
/// (ADR 001) are met via synchronous flushing.
pub trait LogStorage: Send + Sync + Debug {
    // --- Persistent State Accessors ---

    fn current_term(&self) -> Result<Term, LogStorageError>;
    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError>;
    fn last_log_index(&self) -> Result<LogIndex, LogStorageError>;
    fn last_log_term(&self) -> Result<Term, LogStorageError>;

    /// Returns the last committed log index known to this node.
    fn last_committed(&self) -> Result<LogIndex, LogStorageError>;

    /// Retrieves a single log entry by its index.
    ///
    /// Performs a deep copy/deserialization.
    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError>;

    /// Retrieves a range of log entries in the closed interval [start, end].
    ///
    /// Both bounds are inclusive. Performs a deep copy/deserialization.
    fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, LogStorageError>;

    // --- Persistent State Mutations ---

    /// Persists the Raft Hard State (Term and Vote).
    /// MUST perform a synchronous flush to disk.
    fn save_hard_state(&self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError>;

    /// Persists the Raft commit index.
    /// MUST perform a synchronous flush to disk.
    fn save_last_committed(&self, index: LogIndex) -> Result<(), LogStorageError>;

    /// Appends a batch of entries to the log.
    /// MUST perform a synchronous flush to disk.
    fn append_entries(&self, entries: Vec<LogEntry>) -> Result<(), LogStorageError>;

    /// Appends entries AND advances the commit index in a single operation.
    ///
    /// This is an optimization for the log reconciliation path (RAFT-11) to
    /// reduce two sequential flushes to a single atomic flush. The default
    /// implementation falls back to calling `append_entries` then
    /// `save_last_committed` separately. Override this in storage backends
    /// that can atomically combine both operations.
    fn append_entries_and_advance_commit(
        &self,
        entries: Vec<LogEntry>,
        commit_index: LogIndex,
    ) -> Result<(), LogStorageError> {
        self.append_entries(entries)?;
        self.save_last_committed(commit_index)
    }

    /// Truncates the log, removing all entries from `index` to the end.
    /// MUST perform a synchronous flush to disk.
    fn truncate_log(&self, index: LogIndex) -> Result<(), LogStorageError>;

    /// Truncates the log from the beginning up to (and including) the given
    /// index. MUST perform a synchronous flush to disk.
    fn truncate_log_front(&self, up_to_index: LogIndex) -> Result<(), LogStorageError>;

    /// Persists the metadata for the latest state machine snapshot.
    /// MUST perform a synchronous flush to disk.
    fn save_snapshot_metadata(
        &self,
        last_included_index: LogIndex,
        last_included_term: Term,
    ) -> Result<(), LogStorageError>;

    /// Returns the last log index included in the latest snapshot.
    fn last_included_index(&self) -> Result<LogIndex, LogStorageError>;

    /// Returns the Raft term of the last entry included in the latest snapshot.
    fn last_included_term(&self) -> Result<Term, LogStorageError>;
}

/// Persistent implementation of LogStorage using the `sled` database.
///
/// TREE ARCHITECTURE:
/// To prevent key duplication and ensure logical isolation, this backend
/// utilizes separate sled::Tree instances within a single sled::Db:
/// 1. "log": Exclusively for [LogIndex (BE bytes) => LogEntry (Protobuf)]
/// 2. "meta": Exclusively for [Key (String) => Metadata (Binary)]
///
/// The `db` handle is retained to orchestrate synchronous flushes (fsync)
/// across all trees, satisfying the crash-recovery mandates of ADR 001.
#[derive(Debug)]
pub struct SledStorage {
    db: sled::Db,
    log: sled::Tree,
    meta: sled::Tree,
}

impl SledStorage {
    const KEY_HARD_STATE: &'static [u8] = b"hard_state";
    const KEY_LAST_COMMITTED: &'static [u8] = b"last_committed";
    const KEY_SNAPSHOT_INDEX: &'static [u8] = b"snapshot_index";
    const KEY_SNAPSHOT_TERM: &'static [u8] = b"snapshot_term";
    const TREE_LOG: &'static str = "log";
    const TREE_META: &'static str = "meta";

    pub fn new(db: sled::Db) -> Result<Self, LogStorageError> {
        let log = db
            .open_tree(Self::TREE_LOG)
            .map_err(|e| LogStorageError::persistence(format!("Failed to open log tree: {}", e)))?;
        let meta = db.open_tree(Self::TREE_META).map_err(|e| {
            LogStorageError::persistence(format!("Failed to open meta tree: {}", e))
        })?;
        Ok(Self { db, log, meta })
    }

    fn serialize_hard_state(term: Term, vote: Option<NodeId>) -> Vec<u8> {
        HardState::new(term, vote).encode_to_vec()
    }

    fn deserialize_hard_state(data: &[u8]) -> Result<(Term, Option<NodeId>), LogStorageError> {
        let hs = HardState::decode(data).map_err(|e| {
            LogStorageError::deserialization(format!("Failed to decode HardState: {}", e))
        })?;

        let term = Term::new(hs.term);
        let vote = if hs.vote.is_empty() {
            None
        } else {
            Some(hs.vote.parse::<NodeId>().map_err(|e| {
                LogStorageError::deserialization(format!("Invalid NodeId in HardState: {}", e))
            })?)
        };

        Ok((term, vote))
    }
}

impl LogStorage for SledStorage {
    #[instrument(
        name = "storage_read_term",
        target = "clinical::foundation",
        level = "debug",
        skip_all
    )]
    fn current_term(&self) -> Result<Term, LogStorageError> {
        self.meta
            .get(Self::KEY_HARD_STATE)
            .map_err(|e| LogStorageError::persistence(format!("Failed to read hard_state: {}", e)))?
            .map(|d| Self::deserialize_hard_state(&d))
            .transpose()
            .map(|opt| opt.map(|(t, _)| t).unwrap_or(Term::ZERO))
    }

    #[instrument(
        name = "storage_read_vote",
        target = "clinical::foundation",
        level = "debug",
        skip_all
    )]
    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
        self.meta
            .get(Self::KEY_HARD_STATE)
            .map_err(|e| LogStorageError::persistence(format!("Failed to read hard_state: {}", e)))?
            .map(|d| Self::deserialize_hard_state(&d))
            .transpose()
            .map(|opt| opt.and_then(|(_, v)| v))
    }

    #[instrument(
        name = "storage_read_last_index",
        target = "raft::replication",
        level = "debug",
        skip_all
    )]
    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
        let physical_last = self
            .log
            .last()
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read last log entry: {}", e))
            })?
            .map(|(k, _)| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("LogIndex byte conversion failed")
                })?;
                Ok::<LogIndex, LogStorageError>(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()?
            .unwrap_or(LogIndex::ZERO);

        let snapshot_last = self.last_included_index()?;
        Ok(cmp::max(physical_last, snapshot_last))
    }

    #[instrument(
        name = "storage_read_last_term",
        target = "raft::replication",
        level = "debug",
        skip_all
    )]
    fn last_log_term(&self) -> Result<Term, LogStorageError> {
        let physical_last = self
            .log
            .last()
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read last log entry: {}", e))
            })?
            .map(|(_, v)| {
                LogEntry::decode(v.as_ref()).map_err(|e| {
                    LogStorageError::deserialization(format!(
                        "Failed to decode last log entry: {}",
                        e
                    ))
                })
            })
            .transpose()
            .map(|opt| opt.map(|e| Term::new(e.term)).unwrap_or(Term::ZERO))?;

        if physical_last > Term::ZERO {
            Ok(physical_last)
        } else {
            self.last_included_term()
        }
    }

    #[instrument(
        name = "storage_read_committed",
        target = "raft::replication",
        level = "debug",
        skip_all
    )]
    fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
        self.meta
            .get(Self::KEY_LAST_COMMITTED)
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read last_committed: {}", e))
            })?
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("last_committed byte conversion failed")
                })?;
                Ok(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(LogIndex::ZERO))
    }

    #[instrument(name = "storage_read_entry", target = "raft::replication", level = "debug", skip_all, fields(index = %index))]
    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
        let key = index.as_u64().to_be_bytes();
        self.log
            .get(key)
            .map_err(|e| {
                LogStorageError::persistence(format!(
                    "Failed to read log entry at {}: {}",
                    index, e
                ))
            })?
            .map(|v| {
                LogEntry::decode(v.as_ref()).map_err(|e| {
                    LogStorageError::deserialization(format!(
                        "Failed to decode log entry at {}: {}",
                        index, e
                    ))
                })
            })
            .transpose()
    }

    #[instrument(name = "storage_read_range", target = "raft::replication", level = "debug", skip_all, fields(start = %start, end = %end))]
    fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, LogStorageError> {
        if start == LogIndex::ZERO {
            return Err(LogStorageError::invariant(
                "LogIndex is 1-indexed; range start cannot be 0",
            ));
        }

        if start > end {
            return Ok(Vec::new());
        }

        let start_key = start.as_u64().to_be_bytes();
        let end_key = end.as_u64().to_be_bytes();

        let mut entries = Vec::new();
        for res in self.log.range(start_key..=end_key) {
            let (_, v) = res.map_err(|e| {
                LogStorageError::persistence(format!("Log iteration failure: {}", e))
            })?;
            entries.push(LogEntry::decode(v.as_ref()).map_err(|e| {
                LogStorageError::deserialization(format!("Failed to decode log entry: {}", e))
            })?);
        }
        Ok(entries)
    }

    fn save_hard_state(&self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError> {
        let data = Self::serialize_hard_state(term, vote);
        self.meta.insert(Self::KEY_HARD_STATE, data).map_err(|e| {
            LogStorageError::persistence(format!("Failed to persist hard_state: {}", e))
        })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                error = %e,
                "Sled flush failure during hard_state save"
            );
            LogStorageError::persistence(format!("Sled flush failure: {}", e))
        })?;
        Ok(())
    }

    fn save_last_committed(&self, index: LogIndex) -> Result<(), LogStorageError> {
        self.meta
            .insert(Self::KEY_LAST_COMMITTED, &index.as_u64().to_be_bytes())
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to persist last_committed: {}", e))
            })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                error = %e,
                "Sled flush failure during last_committed save"
            );
            LogStorageError::persistence(format!("Sled flush failure: {}", e))
        })?;
        Ok(())
    }

    fn append_entries(&self, entries: Vec<LogEntry>) -> Result<(), LogStorageError> {
        let mut batch = sled::Batch::default();
        for entry in entries {
            let key = entry.index.to_be_bytes();
            let val = entry.encode_to_vec();
            batch.insert(&key, val);
        }
        self.log.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply log append batch: {}", e))
        })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                error = %e,
                "Sled flush failure after log append"
            );
            LogStorageError::persistence(format!("Sled flush failure after log append: {}", e))
        })?;
        Ok(())
    }

    fn append_entries_and_advance_commit(
        &self,
        entries: Vec<LogEntry>,
        commit_index: LogIndex,
    ) -> Result<(), LogStorageError> {
        let mut batch = sled::Batch::default();
        for entry in entries {
            let key = entry.index.to_be_bytes();
            let val = entry.encode_to_vec();
            batch.insert(&key, val);
        }
        self.log.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply log append batch: {}", e))
        })?;
        self.meta
            .insert(
                Self::KEY_LAST_COMMITTED,
                &commit_index.as_u64().to_be_bytes(),
            )
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to persist last_committed: {}", e))
            })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                error = %e,
                "Sled flush failure after combined log append and commit advance"
            );
            LogStorageError::persistence(format!("Sled flush failure: {}", e))
        })?;
        Ok(())
    }

    fn truncate_log(&self, index: LogIndex) -> Result<(), LogStorageError> {
        let last_idx = self.last_log_index()?;
        if index > last_idx {
            return Ok(());
        }

        let mut batch = sled::Batch::default();
        for i in index.as_u64()..=last_idx.as_u64() {
            batch.remove(&i.to_be_bytes());
        }
        self.log.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply log truncation batch: {}", e))
        })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                error = %e,
                "Sled flush failure after log truncation"
            );
            LogStorageError::persistence(format!("Sled flush failure after log truncation: {}", e))
        })?;
        Ok(())
    }

    fn truncate_log_front(&self, up_to_index: LogIndex) -> Result<(), LogStorageError> {
        let mut batch = sled::Batch::default();
        for i in 1..=up_to_index.as_u64() {
            batch.remove(&i.to_be_bytes());
        }
        self.log.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply front truncation batch: {}", e))
        })?;
        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                error = %e,
                "Sled flush failure after front truncation"
            );
            LogStorageError::persistence(format!(
                "Sled flush failure after front truncation: {}",
                e
            ))
        })?;
        Ok(())
    }

    fn save_snapshot_metadata(
        &self,
        last_included_index: LogIndex,
        last_included_term: Term,
    ) -> Result<(), LogStorageError> {
        let mut batch = sled::Batch::default();
        batch.insert(
            Self::KEY_SNAPSHOT_INDEX,
            &last_included_index.as_u64().to_be_bytes(),
        );
        batch.insert(
            Self::KEY_SNAPSHOT_TERM,
            &last_included_term.as_u64().to_be_bytes(),
        );

        self.meta.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply snapshot metadata batch: {}", e))
        })?;

        self.db.flush().map_err(|e| {
            debug!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                error = %e,
                "Sled flush failure during snapshot metadata save"
            );
            LogStorageError::persistence(format!("Sled flush failure: {}", e))
        })?;
        Ok(())
    }

    #[instrument(
        name = "storage_read_snapshot_index",
        target = "raft::compaction",
        level = "debug",
        skip_all
    )]
    fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
        self.meta
            .get(Self::KEY_SNAPSHOT_INDEX)
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read snapshot_index: {}", e))
            })?
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("snapshot_index byte conversion failed")
                })?;
                Ok(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(LogIndex::ZERO))
    }

    #[instrument(
        name = "storage_read_snapshot_term",
        target = "raft::compaction",
        level = "debug",
        skip_all
    )]
    fn last_included_term(&self) -> Result<Term, LogStorageError> {
        self.meta
            .get(Self::KEY_SNAPSHOT_TERM)
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read snapshot_term: {}", e))
            })?
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("snapshot_term byte conversion failed")
                })?;
                Ok(Term::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(Term::ZERO))
    }
}

/// In-memory implementation of LogStorage for testing and initial bootstrap.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemoryStorage {
    state: std::sync::Mutex<MemoryState>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MemoryState {
    current_term: Term,
    voted_for: Option<NodeId>,
    last_committed: LogIndex,
    last_included_index: LogIndex,
    last_included_term: Term,
    /// 1-indexed vector of consensus entries.
    ///
    /// Index 0 in the vector corresponds to LogIndex(1).
    log: Vec<LogEntry>,
}

#[cfg(test)]
impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the state lock, handling potential poisoning according to
    /// the Halt Mandate (ADR 009).
    fn state(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, LogStorageError> {
        self.state.lock().map_err(|_| {
            LogStorageError::invariant("MemoryStorage Mutex is poisoned (Halt Mandate)")
        })
    }
}

#[cfg(test)]
impl LogStorage for MemoryStorage {
    fn current_term(&self) -> Result<Term, LogStorageError> {
        Ok(self.state()?.current_term)
    }

    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
        Ok(self.state()?.voted_for)
    }

    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
        let state = self.state()?;
        let physical_last = state
            .log
            .last()
            .map(|e| LogIndex::new(e.index))
            .unwrap_or(LogIndex::ZERO);
        Ok(cmp::max(physical_last, state.last_included_index))
    }

    fn last_log_term(&self) -> Result<Term, LogStorageError> {
        let state = self.state()?;
        let physical_last = state
            .log
            .last()
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO);

        if physical_last > Term::ZERO {
            Ok(physical_last)
        } else {
            Ok(state.last_included_term)
        }
    }

    fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
        Ok(self.state()?.last_committed)
    }

    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
        if index == LogIndex::ZERO {
            return Ok(None);
        }
        let state = self.state()?;
        // MemoryStorage uses a simple Vec where LogIndex N is at index N-1.
        // We must check if the index is within the bounds of the current (possibly
        // truncated) log.
        if let Some(first) = state.log.first() {
            if index.as_u64() < first.index {
                return Ok(None);
            }
            let offset = (index.as_u64() - first.index) as usize;
            Ok(state.log.get(offset).cloned())
        } else {
            Ok(None)
        }
    }

    fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, LogStorageError> {
        if start == LogIndex::ZERO {
            return Err(LogStorageError::invariant(
                "LogIndex is 1-indexed; range start cannot be 0",
            ));
        }

        if start > end {
            return Ok(Vec::new());
        }

        let state = self.state()?;
        if let Some(first) = state.log.first() {
            if start.as_u64() < first.index {
                return Err(LogStorageError::invariant(format!(
                    "Log range [{}, {}] overlaps with truncated history (log starts at {})",
                    start, end, first.index
                )));
            }
            let start_offset = (start.as_u64() - first.index) as usize;
            let end_offset = (end.as_u64() - first.index) as usize;
            Ok(state
                .log
                .get(start_offset..=end_offset)
                .map(|s| s.to_vec())
                .unwrap_or_default())
        } else {
            Ok(Vec::new())
        }
    }

    fn save_hard_state(&self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        state.current_term = term;
        state.voted_for = vote;
        Ok(())
    }

    fn save_last_committed(&self, index: LogIndex) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        state.last_committed = index;
        Ok(())
    }

    fn append_entries(&self, entries: Vec<LogEntry>) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        for entry in entries {
            let last_idx = state
                .log
                .last()
                .map(|e| LogIndex::new(e.index))
                .unwrap_or(state.last_included_index);
            let expected_idx = (last_idx + 1)?;
            if LogIndex::new(entry.index) != expected_idx {
                return Err(LogStorageError::invariant(format!(
                    "Non-contiguous log append: expected index {}, got {}",
                    expected_idx, entry.index
                )));
            }
            state.log.push(entry);
        }
        Ok(())
    }

    fn truncate_log(&self, index: LogIndex) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        if let Some(first) = state.log.first() {
            let first_idx = first.index;
            if index.as_u64() <= first_idx {
                state.log.clear();
            } else {
                let keep_count = (index.as_u64() - first_idx) as usize;
                state.log.truncate(keep_count);
            }
        }
        Ok(())
    }

    fn truncate_log_front(&self, up_to_index: LogIndex) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        if let Some(first) = state.log.first() {
            let first_idx = first.index;
            if up_to_index.as_u64() >= first_idx {
                let remove_count = (up_to_index.as_u64() - first_idx + 1) as usize;
                let actual_remove = cmp::min(remove_count, state.log.len());
                state.log.drain(0..actual_remove);
            }
        }
        Ok(())
    }

    fn save_snapshot_metadata(
        &self,
        last_included_index: LogIndex,
        last_included_term: Term,
    ) -> Result<(), LogStorageError> {
        let mut state = self.state()?;
        state.last_included_index = last_included_index;
        state.last_included_term = last_included_term;
        Ok(())
    }

    fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
        Ok(self.state()?.last_included_index)
    }

    fn last_included_term(&self) -> Result<Term, LogStorageError> {
        Ok(self.state()?.last_included_term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod sled_storage {
        use tempfile::tempdir;

        use super::*;

        fn setup_storage() -> SledStorage {
            let dir = tempdir().unwrap();
            let db = sled::open(dir.path()).unwrap();
            SledStorage::new(db).unwrap()
        }

        mod save_hard_state {
            use super::*;
            #[test]
            fn persists_term_and_vote_to_disk() {
                let storage = setup_storage();
                let term = Term::new(5);
                let vote = Some(NodeId::try_new(1).unwrap());

                storage.save_hard_state(term, vote).unwrap();

                assert_eq!(storage.current_term().unwrap(), term);
                assert_eq!(storage.voted_for().unwrap(), vote);
            }
        }

        mod save_last_committed {
            use super::*;
            #[test]
            fn persists_commit_index_to_disk() {
                let storage = setup_storage();
                let index = LogIndex::new(42);

                storage.save_last_committed(index).unwrap();

                assert_eq!(storage.last_committed().unwrap(), index);
            }
        }

        mod append_entries {
            use super::*;
            #[test]
            fn stores_multiple_entries_and_updates_last_log_metadata() {
                let storage = setup_storage();
                let entry1 = LogEntry {
                    index: 1,
                    term: 1,
                    data: b"cmd1".to_vec(),
                };
                let entry2 = LogEntry {
                    index: 2,
                    term: 1,
                    data: b"cmd2".to_vec(),
                };

                storage
                    .append_entries(vec![entry1.clone(), entry2.clone()])
                    .unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(2));
                assert_eq!(storage.last_log_term().unwrap(), Term::new(1));
                assert_eq!(
                    storage.read_entry(LogIndex::new(1)).unwrap().unwrap(),
                    entry1
                );
                assert_eq!(
                    storage.read_entry(LogIndex::new(2)).unwrap().unwrap(),
                    entry2
                );
            }
        }

        mod truncate_log {
            use super::*;
            #[test]
            fn removes_entries_from_given_index_onwards() {
                let storage = setup_storage();
                storage
                    .append_entries(vec![
                        LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        },
                        LogEntry {
                            index: 2,
                            term: 1,
                            data: vec![],
                        },
                        LogEntry {
                            index: 3,
                            term: 2,
                            data: vec![],
                        },
                    ])
                    .unwrap();

                storage.truncate_log(LogIndex::new(2)).unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(1));
                assert!(storage.read_entry(LogIndex::new(2)).unwrap().is_none());
                assert!(storage.read_entry(LogIndex::new(3)).unwrap().is_none());
            }
        }

        mod truncate_log_front {
            use super::*;
            #[test]
            fn removes_entries_up_to_given_index() {
                let storage = setup_storage();
                storage
                    .append_entries(vec![
                        LogEntry::new(LogIndex::new(1), Term::new(1), vec![]),
                        LogEntry::new(LogIndex::new(2), Term::new(1), vec![]),
                        LogEntry::new(LogIndex::new(3), Term::new(2), vec![]),
                    ])
                    .unwrap();

                storage.truncate_log_front(LogIndex::new(2)).unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(3));
                assert!(storage.read_entry(LogIndex::new(1)).unwrap().is_none());
                assert!(storage.read_entry(LogIndex::new(2)).unwrap().is_none());
                assert!(storage.read_entry(LogIndex::new(3)).unwrap().is_some());
            }
        }

        mod snapshot_metadata {
            use super::*;
            #[test]
            fn persists_and_retrieves_snapshot_coordinates() {
                let storage = setup_storage();
                let index = LogIndex::new(100);
                let term = Term::new(5);

                storage.save_snapshot_metadata(index, term).unwrap();

                assert_eq!(storage.last_included_index().unwrap(), index);
                assert_eq!(storage.last_included_term().unwrap(), term);
            }

            #[test]
            fn last_log_coordinates_fallback_to_snapshot_when_log_is_empty() {
                let storage = setup_storage();
                let index = LogIndex::new(100);
                let term = Term::new(5);

                storage.save_snapshot_metadata(index, term).unwrap();

                // Log is empty, should return snapshot values
                assert_eq!(storage.last_log_index().unwrap(), index);
                assert_eq!(storage.last_log_term().unwrap(), term);
            }

            #[test]
            fn last_log_coordinates_prefer_log_over_snapshot() {
                let storage = setup_storage();
                storage
                    .save_snapshot_metadata(LogIndex::new(10), Term::new(1))
                    .unwrap();

                storage
                    .append_entries(vec![LogEntry::new(LogIndex::new(11), Term::new(2), vec![])])
                    .unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(11));
                assert_eq!(storage.last_log_term().unwrap(), Term::new(2));
            }
        }

        mod recovery {
            use super::*;
            #[test]
            fn restores_consistent_state_after_restart() {
                let dir = tempdir().unwrap();
                let db_path = dir.path();

                // 1. Initial write
                {
                    let db = sled::open(db_path).unwrap();
                    let storage = SledStorage::new(db).unwrap();
                    storage
                        .save_hard_state(Term::new(10), Some(NodeId::try_new(42).unwrap()))
                        .unwrap();
                    storage
                        .append_entries(vec![
                            LogEntry::new(LogIndex::new(1), Term::new(1), b"data1".to_vec()),
                            LogEntry::new(LogIndex::new(2), Term::new(10), b"data2".to_vec()),
                        ])
                        .unwrap();
                    // DB handle dropped here
                }

                // 2. Recovery and verification
                {
                    let db = sled::open(db_path).unwrap();
                    let storage = SledStorage::new(db).unwrap();

                    assert_eq!(storage.current_term().unwrap(), Term::new(10));
                    assert_eq!(
                        storage.voted_for().unwrap(),
                        Some(NodeId::try_new(42).unwrap())
                    );
                    assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(2));
                    assert_eq!(storage.last_log_term().unwrap(), Term::new(10));
                    assert_eq!(
                        storage.read_entry(LogIndex::new(1)).unwrap().unwrap().data,
                        b"data1"
                    );
                    assert_eq!(
                        storage.read_entry(LogIndex::new(2)).unwrap().unwrap().data,
                        b"data2"
                    );
                }
            }
        }
    }

    mod memory_storage {
        use super::*;

        mod save_hard_state {
            use super::*;
            #[test]
            fn persists_term_and_vote_in_memory() {
                let storage = MemoryStorage::new();
                let term = Term::new(5);
                let vote = Some(NodeId::try_new(1).unwrap());

                storage.save_hard_state(term, vote).unwrap();

                assert_eq!(storage.current_term().unwrap(), term);
                assert_eq!(storage.voted_for().unwrap(), vote);
            }
        }

        mod save_last_committed {
            use super::*;
            #[test]
            fn persists_commit_index_in_memory() {
                let storage = MemoryStorage::new();
                let index = LogIndex::new(42);

                storage.save_last_committed(index).unwrap();

                assert_eq!(storage.last_committed().unwrap(), index);
            }
        }

        mod append_entries {
            use super::*;
            #[test]
            fn stores_entries_contiguously() {
                let storage = MemoryStorage::new();
                let entry1 = LogEntry {
                    index: 1,
                    term: 1,
                    data: b"cmd1".to_vec(),
                };
                let entry2 = LogEntry {
                    index: 2,
                    term: 1,
                    data: b"cmd2".to_vec(),
                };

                storage
                    .append_entries(vec![entry1.clone(), entry2.clone()])
                    .unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(2));
                assert_eq!(
                    storage.read_entry(LogIndex::new(1)).unwrap().unwrap(),
                    entry1
                );
                assert_eq!(
                    storage.read_entry(LogIndex::new(2)).unwrap().unwrap(),
                    entry2
                );
            }
        }

        mod truncate_log {
            use super::*;
            #[test]
            fn removes_entries_from_tail() {
                let storage = MemoryStorage::new();
                storage
                    .append_entries(vec![
                        LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        },
                        LogEntry {
                            index: 2,
                            term: 1,
                            data: vec![],
                        },
                    ])
                    .unwrap();

                storage.truncate_log(LogIndex::new(2)).unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(1));
                assert!(storage.read_entry(LogIndex::new(2)).unwrap().is_none());
            }
        }

        mod truncate_log_front {
            use super::*;
            #[test]
            fn removes_entries_from_head() {
                let storage = MemoryStorage::new();
                storage
                    .append_entries(vec![
                        LogEntry::new(LogIndex::new(1), Term::new(1), vec![]),
                        LogEntry::new(LogIndex::new(2), Term::new(1), vec![]),
                        LogEntry::new(LogIndex::new(3), Term::new(2), vec![]),
                    ])
                    .unwrap();

                storage.truncate_log_front(LogIndex::new(2)).unwrap();

                assert_eq!(storage.last_log_index().unwrap(), LogIndex::new(3));
                assert!(storage.read_entry(LogIndex::new(1)).unwrap().is_none());
                assert!(storage.read_entry(LogIndex::new(2)).unwrap().is_none());
                assert!(storage.read_entry(LogIndex::new(3)).unwrap().is_some());
            }
        }

        mod snapshot_metadata {
            use super::*;
            #[test]
            fn persists_and_retrieves_snapshot_coordinates() {
                let storage = MemoryStorage::new();
                let index = LogIndex::new(100);
                let term = Term::new(5);

                storage.save_snapshot_metadata(index, term).unwrap();

                assert_eq!(storage.last_included_index().unwrap(), index);
                assert_eq!(storage.last_included_term().unwrap(), term);
            }

            #[test]
            fn last_log_coordinates_fallback_to_snapshot_when_log_is_empty() {
                let storage = MemoryStorage::new();
                let index = LogIndex::new(100);
                let term = Term::new(5);

                storage.save_snapshot_metadata(index, term).unwrap();

                assert_eq!(storage.last_log_index().unwrap(), index);
                assert_eq!(storage.last_log_term().unwrap(), term);
            }
        }
    }
}
