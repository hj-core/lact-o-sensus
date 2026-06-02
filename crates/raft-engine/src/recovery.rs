//! Cold-Boot Recovery Orchestration
//!
//! This module implements the "Cold-Boot" recovery phase of the Tri-Layer Onion
//! model (ADR 009). It synchronizes the persistent State Machine with the
//! Consensus Log before the node enters the network or instantiates its
//! logical state.
//!
//! Since this phase occurs in a pre-boot context, terminal invariant violations
//! (causal regressions or missing committed entries) trigger an immediate
//! `panic!` to satisfy the Halt Mandate, as the logical `Poisoned` state
//! is not yet available. Causal integrity is verified against ADR 010.

use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;

use crate::storage::LogStorage;

/// Orchestrates the synchronization of the persistent State Machine with the
/// Raft Consensus Log on node startup.
pub struct RecoveryManager<S: StateMachine> {
    fsm: Arc<S>,
    log_store: Arc<dyn LogStorage>,
}

impl<S: StateMachine> RecoveryManager<S> {
    pub fn new(fsm: Arc<S>, log_store: Arc<dyn LogStorage>) -> Self {
        Self { fsm, log_store }
    }

    /// Replays all committed log entries that have not yet been applied to the
    /// State Machine.
    ///
    /// This method blocks until the FSM reaches the last persisted commit
    /// index.
    pub async fn recover(&self) -> Result<(), NodeError> {
        // ADR 010: Manual span orchestration to use type-safe ClinicalTarget registry.
        let span = info_span!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            "recovery_session"
        );

        async move {
            let last_applied = self.fsm.last_applied_index().map_err(|e| e.into())?;
            let last_committed = self.log_store.last_committed().map_err(NodeError::from)?;

            self.verify_causal_integrity(last_applied, last_committed);

            if last_applied == last_committed {
                info!(
                    target: ClinicalTarget::ClinicalRecovery.as_str(),
                    index = %last_applied,
                    "State is already synchronized. Replay skipped."
                );
                return Ok(());
            }

            self.replay_committed_entries(last_applied, last_committed)?;

            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Validates that the FSM has not progressed beyond the last known
    /// committed index in the log, which would indicate a causal
    /// regression.
    fn verify_causal_integrity(&self, last_applied: LogIndex, last_committed: LogIndex) {
        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            last_applied = %last_applied,
            last_committed = %last_committed,
            "Executing initial causal integrity check"
        );

        if last_applied > last_committed {
            // Rule 3.4: Pre-boot Halt Mandate.
            // Since the LogicalNode doesn't exist yet, we trigger an immediate panic
            // to prevent the node from starting with a corrupted state.
            error!(
                target: ClinicalTarget::ClinicalRecovery.as_str(),
                last_applied = %last_applied,
                last_committed = %last_committed,
                "HALT MANDATE (ADR 009): FSM index ahead of last_committed. Causal regression detected."
            );
            panic!(
                "Secure Clinical: Terminal Invariant Violation: FSM index {} is ahead of \
                 last_committed {}. State drift detected during Cold-Boot.",
                last_applied, last_committed
            );
        }
    }

    /// Iterates through the consensus log and applies all missing committed
    /// entries to the FSM.
    fn replay_committed_entries(
        &self,
        last_applied: LogIndex,
        last_committed: LogIndex,
    ) -> Result<(), NodeError> {
        let start_index = (last_applied + 1)?;

        info!(
            target: ClinicalTarget::ClinicalRecovery.as_str(),
            start_index = %start_index,
            end_index = %last_committed,
            "Commencing log replay"
        );

        let mut current = last_applied;
        while current < last_committed {
            let apply_idx = (current + 1)?;
            let entry = match self.log_store.read_entry(apply_idx)? {
                Some(e) => e,
                None => {
                    // Rule 3.4: Pre-boot Halt Mandate.
                    // A missing committed entry is a fatal data integrity failure.
                    error!(
                        target: ClinicalTarget::ClinicalRecovery.as_str(),
                        index = %apply_idx,
                        "HALT MANDATE (ADR 009): Committed entry missing from log during recovery."
                    );
                    panic!(
                        "Secure Clinical: Terminal Invariant Violation: Committed entry {} \
                         missing from log during Cold-Boot.",
                        apply_idx
                    );
                }
            };

            self.fsm
                .apply(apply_idx, &entry.data)
                .map_err(|e| e.into())?;
            current = apply_idx;

            if current.as_u64().is_multiple_of(100) {
                info!(
                    target: ClinicalTarget::ClinicalRecovery.as_str(),
                    progress = %current,
                    target_index = %last_committed,
                    "Recovery in progress..."
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

    use common::proto::v1::raft::LogEntry;
    use common::types::LogIndex;
    use common::types::Term;
    use common::types::errors::FsmError;
    use common::types::trace::TraceId;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        last_applied: Mutex<LogIndex>,
        fail_apply: Mutex<bool>,
    }

    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(*self.last_applied.lock().unwrap())
        }

        fn apply(&self, index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
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
            async fn handles_single_entry_replay() {
                let fsm = Arc::new(MockFsm::default());
                let storage = MemoryStorage::new();

                storage
                    .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![1])])
                    .unwrap();
                storage.save_last_committed(LogIndex::new(1)).unwrap();

                let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
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

                let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
                recovery.recover().await.expect("Recovery failed");

                let applied = fsm.applied_indices.lock().unwrap();
                assert!(applied.is_empty());
            }
        }

        mod causal_regression_detected {
            use super::*;

            #[tokio::test]
            #[should_panic(expected = "State drift detected during Cold-Boot")]
            async fn panics_and_halts_node() {
                let fsm = Arc::new(MockFsm::default());
                {
                    *fsm.last_applied.lock().unwrap() = LogIndex::new(5);
                }
                let storage = MemoryStorage::new();
                storage.save_last_committed(LogIndex::new(3)).unwrap();

                let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
                let _ = recovery.recover().await;
            }
        }

        mod physical_corruption_encountered {
            use super::*;

            #[tokio::test]
            #[should_panic(expected = "missing from log during Cold-Boot")]
            async fn panics_and_halts_node_if_committed_entry_missing() {
                let fsm = Arc::new(MockFsm::default());
                let storage = MemoryStorage::new();
                storage.save_last_committed(LogIndex::new(1)).unwrap();

                let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
                let _ = recovery.recover().await;
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

                let recovery = RecoveryManager::new(fsm.clone(), Arc::new(storage));
                let result = recovery.recover().await;

                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("FSM simulated failure")
                );
            }
        }
    }
}
