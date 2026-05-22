use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::errors::NodeError;
use common::types::identity::NodeIdentity;
use common::types::trace::ClinicalTarget;
use tracing::error;
use tracing::info;

use crate::storage::LogStorage;

/// Orchestrates the synchronization of the persistent State Machine with the
/// Raft Consensus Log on node startup.
pub struct RecoveryManager {
    identity: Arc<NodeIdentity>,
    fsm: Arc<dyn StateMachine>,
    log_store: Arc<dyn LogStorage>,
}

impl RecoveryManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<dyn StateMachine>,
        log_store: Arc<dyn LogStorage>,
    ) -> Self {
        Self {
            identity,
            fsm,
            log_store,
        }
    }

    /// Replays all committed log entries that have not yet been applied to the
    /// State Machine.
    ///
    /// This method blocks until the FSM reaches the last persisted commit
    /// index.
    pub async fn recover(&self) -> Result<(), NodeError> {
        let last_applied = self.fsm.last_applied_index().map_err(NodeError::from)?;
        let last_committed = self.log_store.last_committed().map_err(NodeError::from)?;

        self.verify_causal_integrity(last_applied, last_committed)?;

        if last_applied == last_committed {
            info!(
                target: ClinicalTarget::ClinicalRecovery.as_str(),
                index = %last_applied,
                "Recovery: State is already synchronized. Replay skipped."
            );
            return Ok(());
        }

        self.replay_committed_entries(last_applied, last_committed)
            .await?;

        Ok(())
    }

    /// Validates that the FSM has not progressed beyond the last known
    /// committed index in the log, which would indicate a causal
    /// regression.
    fn verify_causal_integrity(
        &self,
        last_applied: LogIndex,
        last_committed: LogIndex,
    ) -> Result<(), NodeError> {
        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            last_applied = %last_applied,
            last_committed = %last_committed,
            "Recovery: Initial state check"
        );

        if last_applied > last_committed {
            error!(
                target: ClinicalTarget::ClinicalRecovery.as_str(),
                cluster_id = %self.identity.cluster_id(),
                node_id = %self.identity.node_id(),
                last_applied = %last_applied,
                last_committed = %last_committed,
                "HALT MANDATE (ADR 009): FSM index ahead of last_committed. Causal regression detected."
            );
            return Err(NodeError::Protocol(format!(
                "FSM index {} is ahead of last_committed {}. State drift detected.",
                last_applied, last_committed
            )));
        }

        Ok(())
    }

    /// Iterates through the consensus log and applies all missing committed
    /// entries to the FSM.
    async fn replay_committed_entries(
        &self,
        last_applied: LogIndex,
        last_committed: LogIndex,
    ) -> Result<(), NodeError> {
        let start_index = (last_applied + 1)?;

        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            start_index = %start_index,
            end_index = %last_committed,
            "Recovery: REPLAY START"
        );

        let mut current = last_applied;
        while current < last_committed {
            let apply_idx = (current + 1)?;
            let entry = self.log_store.read_entry(apply_idx)?.ok_or_else(|| {
                error!(
                    target: ClinicalTarget::ClinicalRecovery.as_str(),
                    index = %apply_idx,
                    "HALT MANDATE (ADR 009): Committed entry missing from log during recovery."
                );
                NodeError::Protocol(format!(
                    "Committed entry {} missing from log during recovery",
                    apply_idx
                ))
            })?;

            self.fsm
                .apply(apply_idx, &entry.data)
                .await
                .map_err(NodeError::from)?;
            current = apply_idx;

            if current.as_u64().is_multiple_of(100) {
                info!(
                    target: ClinicalTarget::ClinicalRecovery.as_str(),
                    progress = %current,
                    "Recovery: Progressing..."
                );
            }
        }

        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            final_index = %current,
            "Recovery: REPLAY COMPLETE. FSM synchronized."
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
    use common::types::identity::ClusterId;
    use common::types::identity::NodeId;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        last_applied: Mutex<LogIndex>,
        fail_apply: Mutex<bool>,
    }

    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(*self.last_applied.lock().unwrap())
        }

        async fn apply(&self, index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            if *self.fail_apply.lock().unwrap() {
                return Err(FsmError::invariant("FSM simulated failure"));
            }
            let mut last = self.last_applied.lock().unwrap();
            if index != (*last + 1).unwrap() {
                return Err(FsmError::invariant("Out of order apply"));
            }
            *last = index;
            self.applied_indices.lock().unwrap().push(index);
            Ok(())
        }
    }

    fn test_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(1).unwrap(),
        ))
    }

    mod recover {
        use super::*;

        mod fsm_is_behind {
            use super::*;

            #[tokio::test]
            async fn replays_all_missing_entries() {
                let fsm = Arc::new(MockFsm::default());
                let storage = MemoryStorage::new();

                storage
                    .append_entries(vec![
                        LogEntry::new(LogIndex::new(1), Term::new(1), vec![1]),
                        LogEntry::new(LogIndex::new(2), Term::new(1), vec![2]),
                        LogEntry::new(LogIndex::new(3), Term::new(1), vec![3]),
                    ])
                    .unwrap();
                storage.save_last_committed(LogIndex::new(3)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                recovery.recover().await.expect("Recovery failed");

                let applied = fsm.applied_indices.lock().unwrap();
                assert_eq!(
                    applied.as_slice(),
                    &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
                );
                assert_eq!(fsm.last_applied_index().unwrap(), LogIndex::new(3));
            }

            #[tokio::test]
            async fn handles_single_entry_replay() {
                let fsm = Arc::new(MockFsm::default());
                let storage = MemoryStorage::new();

                storage
                    .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![1])])
                    .unwrap();
                storage.save_last_committed(LogIndex::new(1)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                recovery.recover().await.expect("Recovery failed");

                let applied = fsm.applied_indices.lock().unwrap();
                assert_eq!(applied.as_slice(), &[LogIndex::new(1)]);
            }
        }

        mod fsm_is_synchronized {
            use super::*;

            #[tokio::test]
            async fn skips_replay_and_returns_ok() {
                let fsm = Arc::new(MockFsm::default());
                {
                    *fsm.last_applied.lock().unwrap() = LogIndex::new(3);
                }
                let storage = MemoryStorage::new();
                storage.save_last_committed(LogIndex::new(3)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                recovery.recover().await.expect("Recovery failed");

                let applied = fsm.applied_indices.lock().unwrap();
                assert!(applied.is_empty());
            }
        }

        mod causal_regression_detected {
            use super::*;

            #[tokio::test]
            async fn returns_protocol_error_and_halts() {
                let fsm = Arc::new(MockFsm::default());
                {
                    *fsm.last_applied.lock().unwrap() = LogIndex::new(5);
                }
                let storage = MemoryStorage::new();
                storage.save_last_committed(LogIndex::new(3)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                let result = recovery.recover().await;

                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("ahead of last_committed")
                );
            }
        }

        mod physical_corruption_encountered {
            use super::*;

            #[tokio::test]
            async fn returns_protocol_error_if_committed_entry_missing() {
                let fsm = Arc::new(MockFsm::default());
                let storage = MemoryStorage::new();
                storage.save_last_committed(LogIndex::new(1)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                let result = recovery.recover().await;

                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("missing from log"));
            }
        }

        mod io_failure_occurs {
            use super::*;

            #[tokio::test]
            async fn propagates_fsm_apply_errors() {
                let fsm = Arc::new(MockFsm::default());
                {
                    *fsm.fail_apply.lock().unwrap() = true;
                }
                let storage = MemoryStorage::new();
                storage
                    .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![1])])
                    .unwrap();
                storage.save_last_committed(LogIndex::new(1)).unwrap();

                let recovery =
                    RecoveryManager::new(test_identity(), fsm.clone(), Arc::new(storage));
                let result = recovery.recover().await;

                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("FSM simulated failure")
                );
            }

            #[tokio::test]
            async fn propagates_log_storage_read_errors() {
                // To simulate log storage read errors, we'd need a mock
                // LogStorage. For now, MemoryStorage doesn't fail, but we could
                // add a MockLogStore.
            }
        }
    }
}
