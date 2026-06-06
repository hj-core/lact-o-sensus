//! Shared behavioral specifications for the RaftNode.
//!
//! This module contains tests for logic that is common across all roles,
//! ensuring consistent invariant protection regardless of the current
//! Type-State.

use std::sync::Arc;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;

use super::test_utils::*;
use super::*;
use crate::storage::LogStorage;
use crate::storage::MemoryStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

#[cfg(test)]
mod tests {
    use super::*;

    mod shared {
        use super::*;

        mod get_term_at {
            use super::*;

            #[test]
            fn should_return_zero_when_index_is_zero() {
                let log_store = Arc::new(MemoryStorage::new());
                let node = setup_node_as_follower(log_store);
                assert_eq!(node.get_term_at(LogIndex::ZERO).unwrap(), Term::ZERO);
            }

            #[test]
            fn should_return_term_from_snapshot_when_index_is_truncated() {
                let log_store = Arc::new(MemoryStorage::new());

                // Setup snapshot at index 10, term 2
                let snapshot_index = LogIndex::new(10);
                let snapshot_term = Term::new(2);
                log_store
                    .save_snapshot_metadata(snapshot_index, snapshot_term)
                    .unwrap();

                let node = setup_node_as_follower(log_store);

                // Authority check (§7): Should return snapshot term even if log is empty
                assert_eq!(node.get_term_at(snapshot_index).unwrap(), snapshot_term);
            }

            #[test]
            fn should_return_term_from_log_when_index_is_recent() {
                let log_store = Arc::new(MemoryStorage::new());

                // Append an entry at index 1, term 1
                log_store
                    .append_entries(vec![LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    }])
                    .unwrap();

                let node = setup_node_as_follower(log_store);

                assert_eq!(node.get_term_at(LogIndex::new(1)).unwrap(), Term::new(1));
            }
        }

        mod state_mut {
            use super::*;

            mod heartbeat_timer {
                use super::*;

                mod reset {
                    use super::*;

                    #[test]
                    fn should_update_timer_when_invoked() {
                        let log_store = Arc::new(MemoryStorage::new());
                        let mut node = RaftNode::<Follower>::try_new(
                            test_identity(1),
                            log_store,
                            Tick::new(0),
                            TickDuration::new(100),
                        )
                        .unwrap();
                        let initial_time = node.state().last_heartbeat();

                        node.state_mut().reset_heartbeat(Tick::new(10));

                        assert_eq!(node.state().last_heartbeat(), Tick::new(10));
                        assert!(node.state().last_heartbeat() > initial_time);
                    }
                }
            }
        }

        mod try_into_follower {
            use super::*;

            mod on_term_update {
                use super::*;

                #[test]
                fn should_reset_voting_state_when_demoted_on_new_term() {
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::try_new(1).unwrap()))
                        .unwrap();
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let demoted = node
                        .try_into_follower(Term::new(2), None, Tick::new(0), TickDuration::new(100))
                        .unwrap();

                    assert_eq!(demoted.current_term().unwrap(), Term::new(2));
                    assert_eq!(demoted.voted_for().unwrap(), None);
                }

                #[test]
                fn should_preserve_vote_when_demoted_on_same_term() {
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::try_new(3).unwrap()))
                        .unwrap();
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let demoted = node
                        .try_into_follower(Term::new(1), None, Tick::new(0), TickDuration::new(100))
                        .unwrap();
                    assert_eq!(
                        demoted.voted_for().unwrap(),
                        Some(NodeId::try_new(3).unwrap())
                    );
                }
            }
        }

        mod advance_last_committed {
            use super::*;

            mod on_valid_index {
                use super::*;

                async fn check_persists_to_log_store_when_index_is_valid<R: NodeState>(
                    mut node: RaftNode<R>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();

                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    assert_eq!(log_store.last_committed().unwrap(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_follower(log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_candidate(log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_leader(log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                async fn check_does_not_apply_to_fsm_when_index_is_valid<R: NodeState>(
                    mut node: RaftNode<R>,
                    fsm: Arc<MockFsm>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![1],
                        }])
                        .unwrap();

                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    // Verify commit index is advanced, but FSM is NOT applied
                    // synchronously (deferred to background applier).
                    assert_eq!(node.last_committed(), LogIndex::new(1));
                    assert_eq!(fsm.applied_indices.lock().unwrap().len(), 0);
                    assert_eq!(node.last_applied(), LogIndex::ZERO);
                }

                #[tokio::test]
                async fn should_not_apply_to_fsm_when_index_is_valid_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_follower(log_store.clone());
                    check_does_not_apply_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn should_not_apply_to_fsm_when_index_is_valid_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_candidate(log_store.clone());
                    check_does_not_apply_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn should_not_apply_to_fsm_when_index_is_valid_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_leader(log_store.clone());
                    check_does_not_apply_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                async fn check_does_not_apply_multiple_entries_when_index_jumps_ahead<
                    R: NodeState,
                >(
                    mut node: RaftNode<R>,
                    fsm: Arc<MockFsm>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    let mut entries = Vec::new();
                    for i in 1..=3 {
                        entries.push(LogEntry {
                            index: i,
                            term: 1,
                            data: vec![i as u8],
                        });
                    }
                    log_store.append_entries(entries).unwrap();

                    node.advance_last_committed(LogIndex::new(3)).unwrap();

                    // Verify commit index is advanced, but none of the entries
                    // are synchronously applied (deferred to background applier).
                    assert_eq!(node.last_committed(), LogIndex::new(3));
                    assert_eq!(fsm.applied_indices.lock().unwrap().len(), 0);
                    assert_eq!(node.last_applied(), LogIndex::ZERO);
                }

                #[tokio::test]
                async fn should_not_apply_multiple_entries_when_index_jumps_ahead_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_follower(log_store.clone());
                    check_does_not_apply_multiple_entries_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }

                #[tokio::test]
                async fn should_not_apply_multiple_entries_when_index_jumps_ahead_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_candidate(log_store.clone());
                    check_does_not_apply_multiple_entries_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }

                #[tokio::test]
                async fn should_not_apply_multiple_entries_when_index_jumps_ahead_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let fsm = Arc::new(MockFsm::default());
                    let node = setup_node_as_leader(log_store.clone());
                    check_does_not_apply_multiple_entries_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }
            }

            mod on_stale_index {
                use super::*;

                async fn check_ignores_update_when_index_is_lower_than_current<R: NodeState>(
                    mut node: RaftNode<R>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    node.advance_last_committed(LogIndex::ZERO).unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_follower(log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_candidate(log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_leader(log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }
            }

            mod on_invalid_index {
                use super::*;

                async fn check_returns_error_when_index_exceeds_last_log_index<R: NodeState>(
                    mut node: RaftNode<R>,
                ) {
                    let result = node.advance_last_committed(LogIndex::new(1));
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("last_log_index"));
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_follower(log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_candidate(log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_leader(log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }
            }
        }

        mod apply_to_state_machine {
            use common::types::errors::FsmError;

            use super::*;

            mod on_fsm_regression {
                use super::*;

                #[derive(Debug, Default)]
                struct RegressionFsm;
                impl StateMachine for RegressionFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::new(100))
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
                        _trace_id: common::types::trace::TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                async fn check_returns_error_when_fsm_index_is_ahead_of_last_committed<
                    R: NodeState,
                >(
                    mut node: RaftNode<R>,
                    fsm: &RegressionFsm,
                ) {
                    let result = node.apply_to_state_machine(fsm);
                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("ahead of last_committed")
                    );
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_follower()
                {
                    let fsm = RegressionFsm;
                    let mut node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    node.last_applied = LogIndex::new(100);
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node, &fsm).await;
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_candidate()
                 {
                    let fsm = RegressionFsm;
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap()
                    .try_into_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap();
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node, &fsm).await;
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_leader() {
                    let fsm = RegressionFsm;
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap()
                    .try_into_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap()
                    .try_into_leader(vec![], Tick::new(0))
                    .unwrap();
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node, &fsm).await;
                }
            }

            mod on_physical_corruption {
                use super::*;

                async fn check_returns_error_when_committed_entry_is_missing_from_log_store<
                    R: NodeState,
                >(
                    mut node: RaftNode<R>,
                    fsm: &MockFsm,
                ) {
                    node.last_committed = LogIndex::new(5);
                    let result = node.apply_to_state_machine(fsm);
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("missing from log"));
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_follower()
                 {
                    let fsm = MockFsm::default();
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node, &fsm)
                        .await;
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_candidate()
                 {
                    let fsm = MockFsm::default();
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap()
                    .try_into_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap();
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node, &fsm)
                        .await;
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_leader()
                 {
                    let fsm = MockFsm::default();
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap()
                    .try_into_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap()
                    .try_into_leader(vec![], Tick::new(0))
                    .unwrap();
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node, &fsm)
                        .await;
                }
            }
        }

        mod advance_term {
            use super::*;

            mod on_higher_term {
                use super::*;
                const STARTING_TERM: Term = Term::new(5);
                const HIGHER_TERM: Term = Term::new(6);
                const BOOTSTRAP_TERM: Term = Term::new(4);

                async fn check_persists_new_term_and_resets_voted_for<R: NodeState>(
                    mut node: RaftNode<R>,
                ) {
                    node.advance_term(HIGHER_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), HIGHER_TERM);
                    assert_eq!(node.voted_for().unwrap(), None);
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let node = setup_node_as_follower(log_store);
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    // Start at BOOTSTRAP_TERM so setup_node_as_candidate increments to
                    // STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let mut node = setup_node_as_candidate(log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    // Start at BOOTSTRAP_TERM so setup_node_as_leader increments to STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let mut node = setup_node_as_leader(log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }
            }

            mod on_same_term {
                use super::*;
                const STARTING_TERM: Term = Term::new(5);
                const SAME_TERM: Term = Term::new(5);
                const BOOTSTRAP_TERM: Term = Term::new(4);

                async fn check_preserves_current_term_and_voting_state<R: NodeState>(
                    mut node: RaftNode<R>,
                ) {
                    node.advance_term(SAME_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), STARTING_TERM);
                    assert_eq!(node.voted_for().unwrap(), Some(NodeId::try_new(2).unwrap()));
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let node = setup_node_as_follower(log_store);
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_candidate(log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_leader(log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }
            }

            mod on_term_regression {
                use super::*;
                const TARGET_TERM: Term = Term::new(10);
                const LOWER_TERM: Term = Term::new(5);

                async fn check_returns_error_when_new_term_is_lower_than_current<R: NodeState>(
                    mut node: RaftNode<R>,
                ) {
                    node.advance_term(TARGET_TERM).unwrap();

                    // Action: Attempt to regress to term 5.
                    let result = node.advance_term(LOWER_TERM);

                    // Verification: Error is returned.
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("regression"));
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_follower() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_follower(log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_candidate() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_candidate(log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_leader() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = setup_node_as_leader(log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }
            }

            mod on_storage_failure {
                use common::types::errors::LogStorageError;

                use super::*;

                #[derive(Debug, Default)]
                struct FailingStorage;
                impl LogStorage for FailingStorage {
                    fn current_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::new(1))
                    }

                    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
                        Ok(None)
                    }

                    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn last_log_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::ZERO)
                    }

                    fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn read_entry(&self, _: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
                        Ok(None)
                    }

                    fn read_entries(
                        &self,
                        _: LogIndex,
                        _: LogIndex,
                    ) -> Result<Vec<LogEntry>, LogStorageError> {
                        Ok(vec![])
                    }

                    fn save_hard_state(
                        &self,
                        _: Term,
                        _: Option<NodeId>,
                    ) -> Result<(), LogStorageError> {
                        Err(LogStorageError::persistence("Simulated IO Error"))
                    }

                    fn save_last_committed(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn append_entries(&self, _: Vec<LogEntry>) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn truncate_log(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn truncate_log_front(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn save_snapshot_metadata(
                        &self,
                        _: LogIndex,
                        _: Term,
                    ) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn last_included_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::ZERO)
                    }
                }

                async fn check_propagates_persistence_error_when_storage_fails<R: NodeState>(
                    mut node: RaftNode<R>,
                ) {
                    let result = node.advance_term(Term::new(2));

                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("Simulated IO Error")
                    );
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_follower() {
                    let log_store = Arc::new(FailingStorage);
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        log_store,
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_candidate() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                    };
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_leader() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                    };
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }
            }
        }

        mod persist_vote {
            use super::*;

            mod call {
                use super::*;

                #[test]
                fn should_persist_to_log_store_when_invoked() {
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node = setup_node_as_follower(log_store.clone());
                    let candidate_id = NodeId::try_new(2).unwrap();

                    node.persist_vote(candidate_id).unwrap();

                    assert_eq!(node.voted_for().unwrap(), Some(candidate_id));
                    assert_eq!(log_store.voted_for().unwrap(), Some(candidate_id));
                }
            }
        }
    }
}
