//! Shared test utilities for the Raft engine.
//!
//! Provides mock state machine implementations and helper constructors
//! used across multiple test modules.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::FsmError;
use common::types::errors::LogStorageError;
use common::types::trace::TraceId;
use rand::SeedableRng;

use crate::config::Config;
use crate::engine::LogicalNode;
use crate::shell::ConsensusShell;
use crate::storage::MemoryStorage;
use crate::tick::TickDuration;
use crate::tick::TickThresholds;

#[derive(Debug, Default)]
pub struct MockFsm;

impl StateMachine for MockFsm {
    type Error = FsmError;

    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
        Ok(LogIndex::ZERO)
    }

    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
        Ok(vec![])
    }

    fn install_snapshot(
        &self,
        _index: LogIndex,
        _data: &[u8],
        _trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub fn test_config() -> Config {
    Config {
        cluster_id: ClusterId::try_new("test").unwrap(),
        node_id: NodeId::try_new(1).unwrap(),
        listen_addr: "127.0.0.1:8080".parse().unwrap(),
        data_dir: "data".into(),
        peers: HashMap::new(),
        raft: crate::config::RaftConfig::default(),
        policy: crate::config::PolicyConfig::default(),
    }
}

pub fn mock_shell() -> Arc<ConsensusShell<MockFsm>> {
    let id = Arc::new(NodeIdentity::new(
        ClusterId::try_new("test").unwrap(),
        NodeId::try_new(1).unwrap(),
    ));
    let storage = Arc::new(MemoryStorage::new());
    let thresholds = TickThresholds {
        heartbeat_interval: TickDuration::new(10),
        min_election: TickDuration::new(15),
        max_election: TickDuration::new(30),
    };
    let rng = rand::rngs::StdRng::seed_from_u64(1);
    let node = LogicalNode::try_new(id, Arc::new(MockFsm), storage, thresholds, rng).unwrap();
    Arc::new(ConsensusShell::new(node))
}

/// A storage wrapper that fails on `current_term()` but delegates all other
/// operations to a normal `MemoryStorage`. Useful for testing Halt Mandate
/// behavior when storage fails during term reads.
///
/// The `succeed_count` controls how many calls to `current_term()` succeed
/// before failures begin. A value of 0 means the first call fails.
#[derive(Debug)]
pub struct FailingTermStorage {
    inner: MemoryStorage,
    succeed_count: AtomicU32,
}

impl FailingTermStorage {
    /// Creates a storage that fails immediately on the first `current_term()`
    /// call.
    pub fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            succeed_count: AtomicU32::new(0),
        }
    }

    /// Creates a storage that succeeds for `n` calls to `current_term()`,
    /// then fails on the `n+1`-th call.
    pub fn with_succeed_count(n: u32) -> Self {
        Self {
            inner: MemoryStorage::new(),
            succeed_count: AtomicU32::new(n),
        }
    }
}

impl Default for FailingTermStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::storage::LogStorage for FailingTermStorage {
    fn current_term(&self) -> Result<Term, LogStorageError> {
        let prev = self
            .succeed_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1))
            .unwrap_or(0);
        if prev > 0 {
            self.inner.current_term()
        } else {
            Err(LogStorageError::persistence("simulated storage failure"))
        }
    }

    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
        self.inner.voted_for()
    }

    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
        self.inner.last_log_index()
    }

    fn last_log_term(&self) -> Result<Term, LogStorageError> {
        self.inner.last_log_term()
    }

    fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
        self.inner.last_committed()
    }

    fn read_entry(&self, index: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
        self.inner.read_entry(index)
    }

    fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, LogStorageError> {
        self.inner.read_entries(start, end)
    }

    fn save_hard_state(&self, term: Term, vote: Option<NodeId>) -> Result<(), LogStorageError> {
        self.inner.save_hard_state(term, vote)
    }

    fn save_last_committed(&self, index: LogIndex) -> Result<(), LogStorageError> {
        self.inner.save_last_committed(index)
    }

    fn append_entries(&self, entries: Vec<LogEntry>) -> Result<(), LogStorageError> {
        self.inner.append_entries(entries)
    }

    fn truncate_log(&self, index: LogIndex) -> Result<(), LogStorageError> {
        self.inner.truncate_log(index)
    }

    fn truncate_log_front(&self, up_to_index: LogIndex) -> Result<(), LogStorageError> {
        self.inner.truncate_log_front(up_to_index)
    }

    fn save_snapshot_metadata(
        &self,
        last_included_index: LogIndex,
        last_included_term: Term,
    ) -> Result<(), LogStorageError> {
        self.inner
            .save_snapshot_metadata(last_included_index, last_included_term)
    }

    fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
        self.inner.last_included_index()
    }

    fn last_included_term(&self) -> Result<Term, LogStorageError> {
        self.inner.last_included_term()
    }
}
