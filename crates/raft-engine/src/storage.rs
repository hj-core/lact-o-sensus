use std::fmt::Debug;

use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::LogStorageError;
use prost::Message;

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
    fn commit_index(&self) -> Result<LogIndex, LogStorageError>;

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
    fn save_hard_state(&mut self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError>;

    /// Persists the Raft commit index.
    /// MUST perform a synchronous flush to disk.
    fn save_commit_index(&mut self, index: LogIndex) -> Result<(), LogStorageError>;

    /// Appends a batch of entries to the log.
    /// MUST perform a synchronous flush to disk.
    fn append_entries(&mut self, entries: Vec<LogEntry>) -> Result<(), LogStorageError>;

    /// Truncates the log, removing all entries from `index` to the end.
    /// MUST perform a synchronous flush to disk.
    fn truncate_log(&mut self, index: LogIndex) -> Result<(), LogStorageError>;
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
    const KEY_COMMIT_INDEX: &'static [u8] = b"commit_index";
    const KEY_HARD_STATE: &'static [u8] = b"hard_state";
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

    /// Serializes HardState (§5.1) to manual binary format:
    /// [Term (8 bytes BE)] [HasVote (1 byte)] [VoteNodeId (8 bytes BE if
    /// present)]
    fn serialize_hard_state(term: Term, vote: Option<NodeId>) -> Vec<u8> {
        let mut data = Vec::with_capacity(17);
        data.extend_from_slice(&term.value().to_be_bytes());
        match vote {
            Some(node_id) => {
                data.push(1);
                data.extend_from_slice(&node_id.value().to_be_bytes());
            }
            None => {
                data.push(0);
            }
        }
        data
    }

    fn deserialize_hard_state(data: &[u8]) -> Result<(Term, Option<NodeId>), LogStorageError> {
        if data.len() < 9 {
            return Err(LogStorageError::deserialization(
                "Corrupted HardState: insufficient data length",
            ));
        }
        let term = Term::new(u64::from_be_bytes(data[0..8].try_into().map_err(|_| {
            LogStorageError::deserialization("Term byte conversion failed")
        })?));
        let has_vote = data[8] == 1;
        let vote = if has_vote {
            if data.len() < 17 {
                return Err(LogStorageError::deserialization(
                    "Corrupted HardState: missing vote data",
                ));
            }
            Some(NodeId::new(u64::from_be_bytes(
                data[9..17].try_into().map_err(|_| {
                    LogStorageError::deserialization("NodeId byte conversion failed")
                })?,
            )))
        } else {
            None
        };
        Ok((term, vote))
    }
}

impl LogStorage for SledStorage {
    fn current_term(&self) -> Result<Term, LogStorageError> {
        self.meta
            .get(Self::KEY_HARD_STATE)
            .map_err(|e| LogStorageError::persistence(format!("Failed to read hard_state: {}", e)))?
            .map(|d| Self::deserialize_hard_state(&d))
            .transpose()
            .map(|opt| opt.map(|(t, _)| t).unwrap_or(Term::ZERO))
    }

    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
        self.meta
            .get(Self::KEY_HARD_STATE)
            .map_err(|e| LogStorageError::persistence(format!("Failed to read hard_state: {}", e)))?
            .map(|d| Self::deserialize_hard_state(&d))
            .transpose()
            .map(|opt| opt.and_then(|(_, v)| v))
    }

    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
        self.log
            .last()
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read last log entry: {}", e))
            })?
            .map(|(k, _)| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("LogIndex byte conversion failed")
                })?;
                Ok(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(LogIndex::ZERO))
    }

    fn last_log_term(&self) -> Result<Term, LogStorageError> {
        self.log
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
            .map(|opt| opt.map(|e| Term::new(e.term)).unwrap_or(Term::ZERO))
    }

    fn commit_index(&self) -> Result<LogIndex, LogStorageError> {
        self.meta
            .get(Self::KEY_COMMIT_INDEX)
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to read commit_index: {}", e))
            })?
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().map_err(|_| {
                    LogStorageError::deserialization("commit_index byte conversion failed")
                })?;
                Ok(LogIndex::new(u64::from_be_bytes(bytes)))
            })
            .transpose()
            .map(|opt| opt.unwrap_or(LogIndex::ZERO))
    }

    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
        let key = index.value().to_be_bytes();
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

        let start_key = start.value().to_be_bytes();
        let end_key = end.value().to_be_bytes();

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

    fn save_hard_state(&mut self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError> {
        let data = Self::serialize_hard_state(term, vote);
        self.meta.insert(Self::KEY_HARD_STATE, data).map_err(|e| {
            LogStorageError::persistence(format!("Failed to persist hard_state: {}", e))
        })?;
        self.db
            .flush()
            .map_err(|e| LogStorageError::persistence(format!("Sled flush failure: {}", e)))?;
        Ok(())
    }

    fn save_commit_index(&mut self, index: LogIndex) -> Result<(), LogStorageError> {
        self.meta
            .insert(Self::KEY_COMMIT_INDEX, &index.value().to_be_bytes())
            .map_err(|e| {
                LogStorageError::persistence(format!("Failed to persist commit_index: {}", e))
            })?;
        self.db
            .flush()
            .map_err(|e| LogStorageError::persistence(format!("Sled flush failure: {}", e)))?;
        Ok(())
    }

    fn append_entries(&mut self, entries: Vec<LogEntry>) -> Result<(), LogStorageError> {
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
            LogStorageError::persistence(format!("Sled flush failure after log append: {}", e))
        })?;
        Ok(())
    }

    fn truncate_log(&mut self, index: LogIndex) -> Result<(), LogStorageError> {
        let last_idx = self.last_log_index()?;
        if index > last_idx {
            return Ok(());
        }

        let mut batch = sled::Batch::default();
        for i in index.value()..=last_idx.value() {
            batch.remove(&i.to_be_bytes());
        }
        self.log.apply_batch(batch).map_err(|e| {
            LogStorageError::persistence(format!("Failed to apply log truncation batch: {}", e))
        })?;
        self.db.flush().map_err(|e| {
            LogStorageError::persistence(format!("Sled flush failure after log truncation: {}", e))
        })?;
        Ok(())
    }
}

/// In-memory implementation of LogStorage for testing and initial bootstrap.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct MemoryStorage {
    current_term: Term,
    voted_for: Option<NodeId>,
    commit_index: LogIndex,
    /// 1-indexed vector of consensus entries.
    ///
    /// Index 0 in the vector corresponds to LogIndex(1).
    log: Vec<LogEntry>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self {
            current_term: Term::ZERO,
            voted_for: None,
            commit_index: LogIndex::ZERO,
            log: Vec::new(),
        }
    }
}

impl MemoryStorage {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new() -> Self {
        Self::default()
    }
}

impl LogStorage for MemoryStorage {
    fn current_term(&self) -> Result<Term, LogStorageError> {
        Ok(self.current_term)
    }

    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
        Ok(self.voted_for)
    }

    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
        Ok(self
            .log
            .last()
            .map(|e| LogIndex::new(e.index))
            .unwrap_or(LogIndex::ZERO))
    }

    fn last_log_term(&self) -> Result<Term, LogStorageError> {
        Ok(self
            .log
            .last()
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO))
    }

    fn commit_index(&self) -> Result<LogIndex, LogStorageError> {
        Ok(self.commit_index)
    }

    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
        if index == LogIndex::ZERO {
            return Ok(None);
        }
        Ok(self.log.get((index.value() - 1) as usize).cloned())
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

        let start_idx = (start.value() - 1) as usize;
        let end_idx = (end.value() - 1) as usize;
        Ok(self
            .log
            .get(start_idx..=end_idx)
            .map(|s| s.to_vec())
            .unwrap_or_default())
    }

    fn save_hard_state(&mut self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError> {
        self.current_term = term;
        self.voted_for = vote;
        Ok(())
    }

    fn save_commit_index(&mut self, index: LogIndex) -> Result<(), LogStorageError> {
        self.commit_index = index;
        Ok(())
    }

    fn append_entries(&mut self, entries: Vec<LogEntry>) -> Result<(), LogStorageError> {
        for entry in entries {
            let expected_idx = self.last_log_index()? + 1;
            if LogIndex::new(entry.index) != expected_idx {
                return Err(LogStorageError::invariant(format!(
                    "Non-contiguous log append: expected index {}, got {}",
                    expected_idx, entry.index
                )));
            }
            self.log.push(entry);
        }
        Ok(())
    }

    fn truncate_log(&mut self, index: LogIndex) -> Result<(), LogStorageError> {
        if index == LogIndex::ZERO {
            self.log.clear();
        } else {
            self.log.truncate((index.value() - 1) as usize);
        }
        Ok(())
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

        #[test]
        fn persists_hard_state() {
            let mut storage = setup_storage();
            let term = Term::new(5);
            let vote = Some(NodeId::new(1));

            storage.save_hard_state(term, vote).unwrap();

            assert_eq!(storage.current_term().unwrap(), term);
            assert_eq!(storage.voted_for().unwrap(), vote);
        }

        #[test]
        fn persists_commit_index() {
            let mut storage = setup_storage();
            let index = LogIndex::new(42);

            storage.save_commit_index(index).unwrap();

            assert_eq!(storage.commit_index().unwrap(), index);
        }

        #[test]
        fn appends_and_retrieves_entries() {
            let mut storage = setup_storage();
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

        #[test]
        fn truncates_log_at_given_index() {
            let mut storage = setup_storage();
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

        #[test]
        fn survives_restart_with_consistent_state() {
            let dir = tempdir().unwrap();
            let db_path = dir.path();

            // 1. Initial write
            {
                let db = sled::open(db_path).unwrap();
                let mut storage = SledStorage::new(db).unwrap();
                storage
                    .save_hard_state(Term::new(10), Some(NodeId::new(42)))
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
                assert_eq!(storage.voted_for().unwrap(), Some(NodeId::new(42)));
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

    mod memory_storage {
        use super::*;

        #[test]
        fn persists_hard_state() {
            let mut storage = MemoryStorage::new();
            let term = Term::new(5);
            let vote = Some(NodeId::new(1));

            storage.save_hard_state(term, vote).unwrap();

            assert_eq!(storage.current_term().unwrap(), term);
            assert_eq!(storage.voted_for().unwrap(), vote);
        }

        #[test]
        fn persists_commit_index() {
            let mut storage = MemoryStorage::new();
            let index = LogIndex::new(42);

            storage.save_commit_index(index).unwrap();

            assert_eq!(storage.commit_index().unwrap(), index);
        }

        #[test]
        fn appends_and_retrieves_entries() {
            let mut storage = MemoryStorage::new();
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

        #[test]
        fn truncates_log_at_given_index() {
            let mut storage = MemoryStorage::new();
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
        }
    }
}
