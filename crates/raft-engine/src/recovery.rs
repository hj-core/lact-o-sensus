use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::errors::NodeError;
use tracing::error;
use tracing::info;

use crate::storage::LogStorage;

/// Orchestrates the synchronization of the persistent State Machine with the
/// Raft Consensus Log on node startup.
pub struct RecoveryManager {
    fsm: Arc<dyn StateMachine>,
    storage: Arc<dyn LogStorage>,
}

impl RecoveryManager {
    pub fn new(fsm: Arc<dyn StateMachine>, storage: Arc<dyn LogStorage>) -> Self {
        Self { fsm, storage }
    }

    /// Replays all committed log entries that have not yet been applied to the
    /// State Machine.
    ///
    /// This method blocks until the FSM reaches the last persisted commit
    /// index.
    pub async fn recover(&self) -> Result<(), NodeError> {
        let last_applied = self.fsm.last_applied_index().map_err(NodeError::from)?;
        let commit_index = self.storage.commit_index().map_err(NodeError::from)?;

        info!(
            "Recovery: Initial state check [FSM: {}, Log Commit: {}]",
            last_applied, commit_index
        );

        if last_applied > commit_index {
            error!(
                "HALT MANDATE (ADR 009): FSM index {} is ahead of commit_index {}. This indicates \
                 log regression or catastrophic storage corruption.",
                last_applied, commit_index
            );
            return Err(NodeError::Logical(format!(
                "FSM index {} is ahead of commit_index {}. State drift detected.",
                last_applied, commit_index
            )));
        }

        if last_applied == commit_index {
            info!(
                "Recovery: State is already synchronized at index {}. Replay skipped.",
                last_applied
            );
            return Ok(());
        }

        info!(
            "Recovery: REPLAY START [Range: {} -> {}]",
            last_applied + 1,
            commit_index
        );

        let mut current = last_applied;
        while current < commit_index {
            let apply_idx = current + 1;
            let entry = self.storage.read_entry(apply_idx)?.ok_or_else(|| {
                error!(
                    "HALT MANDATE (ADR 009): Committed entry {} missing from log during recovery.",
                    apply_idx
                );
                NodeError::Logical(format!(
                    "Committed entry {} missing from log during recovery",
                    apply_idx
                ))
            })?;

            self.fsm
                .apply(apply_idx, &entry.data)
                .await
                .map_err(NodeError::from)?;
            current = apply_idx;

            if current.value() % 100 == 0 {
                info!("Recovery: Progress ... {} applied", current);
            }
        }

        info!(
            "Recovery: REPLAY COMPLETE. FSM synchronized at index {}.",
            current
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use common::proto::v1::raft::LogEntry;
    use common::types::LogIndex;
    use common::types::Term;
    use common::types::errors::FsmError;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        last_applied: Mutex<LogIndex>,
    }

    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(*self.last_applied.lock().unwrap())
        }

        async fn apply(&self, index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            let mut last = self.last_applied.lock().unwrap();
            if index != *last + 1 {
                return Err(FsmError::invariant("Out of order apply"));
            }
            *last = index;
            self.applied_indices.lock().unwrap().push(index);
            Ok(())
        }
    }

    mod recover {
        use super::*;

        #[tokio::test]
        async fn replays_missing_entries_when_fsm_is_behind() {
            let fsm = Arc::new(MockFsm::default());
            let mut storage = MemoryStorage::new();

            // Setup: Log has 3 entries, commit_index is 3, FSM is at 0.
            storage
                .append_entries(vec![
                    LogEntry::new(LogIndex::new(1), Term::new(1), vec![1]),
                    LogEntry::new(LogIndex::new(2), Term::new(1), vec![2]),
                    LogEntry::new(LogIndex::new(3), Term::new(1), vec![3]),
                ])
                .unwrap();
            storage.save_commit_index(LogIndex::new(3)).unwrap();

            let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
            recovery.recover().await.expect("Recovery failed");

            let applied = fsm.applied_indices.lock().unwrap();
            assert_eq!(
                applied.as_slice(),
                &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
            );
            assert_eq!(fsm.last_applied_index().unwrap(), LogIndex::new(3));
        }

        #[tokio::test]
        async fn performs_no_op_when_fsm_is_synchronized() {
            let fsm = Arc::new(MockFsm::default());
            {
                *fsm.last_applied.lock().unwrap() = LogIndex::new(3);
            }
            let mut storage = MemoryStorage::new();
            storage.save_commit_index(LogIndex::new(3)).unwrap();

            let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
            recovery.recover().await.expect("Recovery failed");

            let applied = fsm.applied_indices.lock().unwrap();
            assert!(applied.is_empty());
        }

        #[tokio::test]
        async fn returns_error_when_fsm_is_ahead_of_log() {
            let fsm = Arc::new(MockFsm::default());
            {
                *fsm.last_applied.lock().unwrap() = LogIndex::new(5);
            }
            let mut storage = MemoryStorage::new();
            storage.save_commit_index(LogIndex::new(3)).unwrap();

            let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
            let result = recovery.recover().await;

            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("ahead of commit_index")
            );
        }

        #[tokio::test]
        async fn returns_error_when_committed_entry_is_missing() {
            let fsm = Arc::new(MockFsm::default());
            let mut storage = MemoryStorage::new();
            // commit_index is 1, but log is empty
            storage.save_commit_index(LogIndex::new(1)).unwrap();

            let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
            let result = recovery.recover().await;

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("missing from log"));
        }
    }
}
