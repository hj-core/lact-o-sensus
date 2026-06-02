use std::result::Result;
use std::sync::Mutex;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::FsmError;
use common::types::errors::NodeError;
use common::types::trace::TraceId;

use super::*;
use crate::storage::LogStorage;
use crate::storage::MemoryStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

#[derive(Debug, Default)]
struct MockFsm {
    applied_indices: Mutex<Vec<LogIndex>>,
    applied_data: Mutex<Vec<Vec<u8>>>,
}

impl StateMachine for MockFsm {
    type Error = FsmError;

    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
        Ok(LogIndex::ZERO)
    }

    fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Self::Error> {
        self.applied_indices
            .lock()
            .expect("Clinical Invariant: Mutex must be lockable")
            .push(index);
        self.applied_data
            .lock()
            .expect("Clinical Invariant: Mutex must be lockable")
            .push(data.to_vec());
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
        Ok(vec![])
    }

    fn install_snapshot(
        &self,
        _last_included_index: LogIndex,
        _data: &[u8],
        _trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn test_identity(id: u64) -> Arc<NodeIdentity> {
    Arc::new(NodeIdentity::new(
        ClusterId::try_new("test-cluster").unwrap(),
        NodeId::try_new(id).unwrap(),
    ))
}

fn setup_node_as_follower(
    fsm: Arc<MockFsm>,
    log_store: Arc<MemoryStorage>,
) -> RaftNode<Follower, MockFsm> {
    RaftNode::try_new(
        test_identity(1),
        fsm,
        log_store,
        Tick::new(0),
        TickDuration::new(100),
    )
    .unwrap()
}

fn setup_node_as_candidate(
    fsm: Arc<MockFsm>,
    log_store: Arc<MemoryStorage>,
) -> RaftNode<Candidate, MockFsm> {
    setup_node_as_follower(fsm, log_store)
        .try_into_candidate(Tick::new(0), TickDuration::new(150))
        .unwrap()
}

fn setup_node_as_leader(
    fsm: Arc<MockFsm>,
    log_store: Arc<MemoryStorage>,
) -> RaftNode<Leader, MockFsm> {
    setup_node_as_candidate(fsm, log_store)
        .try_into_leader(vec![], Tick::new(0))
        .unwrap()
}

mod shared {
    use super::*;

    mod get_term_at {
        use super::*;

        #[test]
        fn should_return_zero_when_index_is_zero() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let node = setup_node_as_follower(fsm, log_store);
            assert_eq!(node.get_term_at(LogIndex::ZERO).unwrap(), Term::ZERO);
        }

        #[test]
        fn should_return_term_from_snapshot_when_index_is_truncated() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());

            // Setup snapshot at index 10, term 2
            let snapshot_index = LogIndex::new(10);
            let snapshot_term = Term::new(2);
            log_store
                .save_snapshot_metadata(snapshot_index, snapshot_term)
                .unwrap();

            let node = setup_node_as_follower(fsm, log_store);

            // Authority check (§7): Should return snapshot term even if log is empty
            assert_eq!(node.get_term_at(snapshot_index).unwrap(), snapshot_term);
        }

        #[test]
        fn should_return_term_from_log_when_index_is_recent() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());

            // Append an entry at index 1, term 1
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![],
                }])
                .unwrap();

            let node = setup_node_as_follower(fsm, log_store);

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
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
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
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                log_store
                    .save_hard_state(Term::new(1), Some(NodeId::try_new(1).unwrap()))
                    .unwrap();
                let node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
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
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                log_store
                    .save_hard_state(Term::new(1), Some(NodeId::try_new(3).unwrap()))
                    .unwrap();
                let node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
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

            async fn check_persists_to_log_store_when_index_is_valid<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
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
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm, log_store.clone());
                check_persists_to_log_store_when_index_is_valid(node, log_store).await;
            }

            #[tokio::test]
            async fn should_persist_to_log_store_when_index_is_valid_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm, log_store.clone());
                check_persists_to_log_store_when_index_is_valid(node, log_store).await;
            }

            #[tokio::test]
            async fn should_persist_to_log_store_when_index_is_valid_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm, log_store.clone());
                check_persists_to_log_store_when_index_is_valid(node, log_store).await;
            }

            async fn check_applies_to_fsm_when_index_is_valid<R: NodeState, S: StateMachine>(
                mut node: RaftNode<R, S>,
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

                assert_eq!(node.last_committed(), LogIndex::new(1));
                assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
            }

            #[tokio::test]
            async fn should_apply_to_fsm_when_index_is_valid_as_follower() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm.clone(), log_store.clone());
                check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
            }

            #[tokio::test]
            async fn should_apply_to_fsm_when_index_is_valid_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm.clone(), log_store.clone());
                check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
            }

            #[tokio::test]
            async fn should_apply_to_fsm_when_index_is_valid_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm.clone(), log_store.clone());
                check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
            }

            async fn check_applies_multiple_entries_sequentially_when_index_jumps_ahead<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
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

                let applied = fsm.applied_indices.lock().unwrap();
                assert_eq!(
                    applied.as_slice(),
                    &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
                );
                assert_eq!(node.last_applied(), LogIndex::new(3));
            }

            #[tokio::test]
            async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_follower()
             {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm.clone(), log_store.clone());
                check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                    node, fsm, log_store,
                )
                .await;
            }

            #[tokio::test]
            async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_candidate()
             {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm.clone(), log_store.clone());
                check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                    node, fsm, log_store,
                )
                .await;
            }

            #[tokio::test]
            async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_leader()
             {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm.clone(), log_store.clone());
                check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                    node, fsm, log_store,
                )
                .await;
            }
        }

        mod on_stale_index {
            use super::*;

            async fn check_ignores_update_when_index_is_lower_than_current<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
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
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm, log_store.clone());
                check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
            }

            #[tokio::test]
            async fn should_ignore_update_when_index_is_lower_than_current_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm, log_store.clone());
                check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
            }

            #[tokio::test]
            async fn should_ignore_update_when_index_is_lower_than_current_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm, log_store.clone());
                check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
            }
        }

        mod on_invalid_index {
            use super::*;

            async fn check_returns_error_when_index_exceeds_last_log_index<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
            ) {
                let result = node.advance_last_committed(LogIndex::new(1));
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("last_log_index"));
            }

            #[tokio::test]
            async fn should_return_error_when_index_exceeds_last_log_index_as_follower() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm, log_store);
                check_returns_error_when_index_exceeds_last_log_index(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_index_exceeds_last_log_index_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm, log_store);
                check_returns_error_when_index_exceeds_last_log_index(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_index_exceeds_last_log_index_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm, log_store);
                check_returns_error_when_index_exceeds_last_log_index(node).await;
            }
        }

        mod on_fsm_failure {
            use super::*;

            #[derive(Debug, Default)]
            struct PoisonFsm;
            impl StateMachine for PoisonFsm {
                type Error = FsmError;

                fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                    Ok(LogIndex::ZERO)
                }

                fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                    Err(FsmError::invariant("Simulated FSM failure"))
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

            async fn check_returns_error_when_state_machine_apply_fails<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
                log_store: Arc<MemoryStorage>,
            ) {
                log_store
                    .append_entries(vec![LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![1],
                    }])
                    .unwrap();

                let result = node.advance_last_committed(LogIndex::new(1));
                assert!(result.is_err());
                let err = result.unwrap_err();
                assert!(
                    matches!(err, NodeError::Protocol(_)),
                    "Expected NodeError::Protocol, got {:?}",
                    err
                );
                assert!(err.to_string().contains("Simulated FSM failure"));
            }

            #[tokio::test]
            async fn should_return_error_when_state_machine_apply_fails_as_follower() {
                let (fsm, log_store) = (Arc::new(PoisonFsm), Arc::new(MemoryStorage::new()));
                let node = RaftNode::<Follower, PoisonFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store.clone(),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                check_returns_error_when_state_machine_apply_fails(node, log_store).await;
            }

            #[tokio::test]
            async fn should_return_error_when_state_machine_apply_fails_as_candidate() {
                let fsm = Arc::new(PoisonFsm);
                let log_store = Arc::new(MemoryStorage::new());
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm,
                    log_store: log_store.clone(),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                };
                check_returns_error_when_state_machine_apply_fails(node, log_store).await;
            }

            #[tokio::test]
            async fn should_return_error_when_state_machine_apply_fails_as_leader() {
                let fsm = Arc::new(PoisonFsm);
                let log_store = Arc::new(MemoryStorage::new());
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm,
                    log_store: log_store.clone(),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                };
                check_returns_error_when_state_machine_apply_fails(node, log_store).await;
            }
        }
    }

    mod apply_to_state_machine {
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
                    _trace_id: TraceId,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            async fn check_returns_error_when_fsm_index_is_ahead_of_last_committed<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
            ) {
                let result = node.apply_to_state_machine();
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
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm: Arc::new(RegressionFsm),
                    log_store: Arc::new(MemoryStorage::new()),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::new(100),
                    state: Follower::new(None, Tick::new(0), TickDuration::new(100)),
                };
                check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_candidate()
             {
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm: Arc::new(RegressionFsm),
                    log_store: Arc::new(MemoryStorage::new()),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                };
                check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_leader() {
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm: Arc::new(RegressionFsm),
                    log_store: Arc::new(MemoryStorage::new()),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                };
                check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
            }
        }

        mod on_physical_corruption {
            use super::*;

            async fn check_returns_error_when_committed_entry_is_missing_from_log_store<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
            ) {
                node.last_committed = LogIndex::new(5);
                let result = node.apply_to_state_machine();
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("missing from log"));
            }

            #[tokio::test]
            async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_follower()
             {
                let node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    Arc::new(MockFsm::default()),
                    Arc::new(MemoryStorage::new()),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_candidate()
             {
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm: Arc::new(MockFsm::default()),
                    log_store: Arc::new(MemoryStorage::new()),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                };
                check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_leader()
             {
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm: Arc::new(MockFsm::default()),
                    log_store: Arc::new(MemoryStorage::new()),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                };
                check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
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

            async fn check_persists_new_term_and_resets_voted_for<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
            ) {
                node.advance_term(HIGHER_TERM).unwrap();
                assert_eq!(node.current_term().unwrap(), HIGHER_TERM);
                assert_eq!(node.voted_for().unwrap(), None);
            }

            #[tokio::test]
            async fn should_persist_new_term_and_reset_voted_for_as_follower() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                log_store
                    .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                    .unwrap();
                let node = setup_node_as_follower(fsm, log_store);
                check_persists_new_term_and_resets_voted_for(node).await;
            }

            #[tokio::test]
            async fn should_persist_new_term_and_reset_voted_for_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                // Start at BOOTSTRAP_TERM so setup_node_as_candidate increments to
                // STARTING_TERM.
                log_store
                    .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                    .unwrap();
                let mut node = setup_node_as_candidate(fsm, log_store);
                // Manually inject a vote for someone else to verify clearing.
                node.advance_term(STARTING_TERM).unwrap();
                node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                check_persists_new_term_and_resets_voted_for(node).await;
            }

            #[tokio::test]
            async fn should_persist_new_term_and_reset_voted_for_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                // Start at BOOTSTRAP_TERM so setup_node_as_leader increments to STARTING_TERM.
                log_store
                    .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                    .unwrap();
                let mut node = setup_node_as_leader(fsm, log_store);
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

            async fn check_preserves_current_term_and_voting_state<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
            ) {
                node.advance_term(SAME_TERM).unwrap();
                assert_eq!(node.current_term().unwrap(), STARTING_TERM);
                assert_eq!(node.voted_for().unwrap(), Some(NodeId::try_new(2).unwrap()));
            }

            #[tokio::test]
            async fn should_preserve_current_term_and_voting_state_as_follower() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                log_store
                    .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                    .unwrap();
                let node = setup_node_as_follower(fsm, log_store);
                check_preserves_current_term_and_voting_state(node).await;
            }

            #[tokio::test]
            async fn should_preserve_current_term_and_voting_state_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                // setup results in STARTING_TERM.
                log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                let mut node = setup_node_as_candidate(fsm, log_store);
                // Ensure voted_for is node 2 as expected by check.
                node.advance_term(STARTING_TERM).unwrap();
                node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                check_preserves_current_term_and_voting_state(node).await;
            }

            #[tokio::test]
            async fn should_preserve_current_term_and_voting_state_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                // setup results in STARTING_TERM.
                log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                let mut node = setup_node_as_leader(fsm, log_store);
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

            async fn check_returns_error_when_new_term_is_lower_than_current<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
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
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_follower(fsm, log_store);
                check_returns_error_when_new_term_is_lower_than_current(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_new_term_is_lower_than_current_as_candidate() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_candidate(fsm, log_store);
                check_returns_error_when_new_term_is_lower_than_current(node).await;
            }

            #[tokio::test]
            async fn should_return_error_when_new_term_is_lower_than_current_as_leader() {
                let (fsm, log_store) =
                    (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                let node = setup_node_as_leader(fsm, log_store);
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

            async fn check_propagates_persistence_error_when_storage_fails<
                R: NodeState,
                S: StateMachine,
            >(
                mut node: RaftNode<R, S>,
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
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(FailingStorage);
                let node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                check_propagates_persistence_error_when_storage_fails(node).await;
            }

            #[tokio::test]
            async fn should_propagate_persistence_error_when_storage_fails_as_candidate() {
                let fsm = Arc::new(MockFsm::default());
                let _node = RaftNode {
                    identity: test_identity(1),
                    fsm: fsm.clone(),
                    log_store: Arc::new(FailingStorage),
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                };
                check_propagates_persistence_error_when_storage_fails(_node).await;
            }

            #[tokio::test]
            async fn should_propagate_persistence_error_when_storage_fails_as_leader() {
                let fsm = Arc::new(MockFsm::default());
                let node = RaftNode {
                    identity: test_identity(1),
                    fsm,
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
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_follower(fsm, log_store.clone());
                let candidate_id = NodeId::try_new(2).unwrap();

                node.persist_vote(candidate_id).unwrap();

                assert_eq!(node.voted_for().unwrap(), Some(candidate_id));
                assert_eq!(log_store.voted_for().unwrap(), Some(candidate_id));
            }
        }
    }
}

mod follower {
    use super::*;

    mod try_new {
        use super::*;
        use crate::storage::SledStorage;

        mod on_causal_recovery {
            use super::*;

            #[test]
            fn should_recover_state_from_log_store_on_initialization() {
                let fsm = Arc::new(MockFsm::default());
                let dir = tempfile::tempdir().unwrap();

                {
                    let db = sled::open(dir.path()).unwrap();
                    let log_store = SledStorage::new(db).unwrap();
                    log_store
                        .save_hard_state(Term::new(7), Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                }

                {
                    let db = sled::open(dir.path()).unwrap();
                    let log_store = Arc::new(SledStorage::new(db).unwrap());
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        log_store,
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    assert_eq!(node.current_term().unwrap(), Term::new(7));
                    assert_eq!(node.voted_for().unwrap(), Some(NodeId::try_new(2).unwrap()));
                }
            }
        }

        mod on_causal_divergence {
            use super::*;

            #[derive(Debug, Default)]
            struct AheadFsm;
            impl StateMachine for AheadFsm {
                type Error = FsmError;

                fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                    Ok(LogIndex::new(100))
                }

                fn apply(&self, _: LogIndex, _: &[u8]) -> Result<(), Self::Error> {
                    Ok(())
                }

                fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                    Ok(vec![])
                }

                fn install_snapshot(
                    &self,
                    _: LogIndex,
                    _: &[u8],
                    _: TraceId,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            #[test]
            fn should_return_error_when_fsm_is_ahead_of_log_store() {
                let fsm = Arc::new(AheadFsm);
                let log_store = Arc::new(MemoryStorage::new());

                let result = RaftNode::<Follower, AheadFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                );

                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Causal invariant violation")
                );
            }
        }
    }

    mod reconcile_log {
        use super::*;

        mod on_consistency_mismatch {
            use super::*;

            #[tokio::test]
            async fn should_reject_append_entries_when_prev_index_is_inconsistent() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                let result = node
                    .reconcile_log(LogIndex::new(1), Term::new(1), vec![], LogIndex::ZERO)
                    .unwrap();
                assert!(!result.success);
            }
        }

        mod on_conflicting_entries {
            use super::*;

            #[tokio::test]
            async fn should_detect_conflicts_and_truncate_log() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                log_store
                    .append_entries(vec![
                        LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![1],
                        },
                        LogEntry {
                            index: 2,
                            term: 1,
                            data: vec![2],
                        },
                    ])
                    .unwrap();
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                let new_entry = LogEntry {
                    index: 2,
                    term: 2,
                    data: vec![3],
                };
                let result = node
                    .reconcile_log(
                        LogIndex::new(1),
                        Term::new(1),
                        vec![new_entry],
                        LogIndex::ZERO,
                    )
                    .unwrap();

                assert!(result.success);
                assert_eq!(result.last_index, LogIndex::new(2));
                assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
            }

            #[tokio::test]
            async fn should_truncate_conflicting_suffix() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                let mut entries = Vec::new();
                for i in 1..=3 {
                    entries.push(LogEntry {
                        index: i,
                        term: 1,
                        data: vec![],
                    });
                }
                log_store.append_entries(entries).unwrap();
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                let new_entry = LogEntry {
                    index: 2,
                    term: 2,
                    data: vec![],
                };
                let result = node
                    .reconcile_log(
                        LogIndex::new(1),
                        Term::new(1),
                        vec![new_entry],
                        LogIndex::ZERO,
                    )
                    .unwrap();

                assert!(result.success);
                assert_eq!(result.last_index, LogIndex::new(2));
                assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
                assert_eq!(node.last_log_index().unwrap(), LogIndex::new(2));
            }
        }

        mod on_duplicate_entries {
            use super::*;

            #[tokio::test]
            async fn should_be_idempotent_when_duplicate_entries_received() {
                let fsm = Arc::new(MockFsm::default());
                let entry = LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![1],
                };
                let log_store = MemoryStorage::new();
                log_store.append_entries(vec![entry.clone()]).unwrap();
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                let result = node
                    .reconcile_log(LogIndex::ZERO, Term::ZERO, vec![entry], LogIndex::ZERO)
                    .unwrap();

                assert!(result.success);
                assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
            }
        }

        mod on_non_contiguous_append {
            use super::*;

            #[tokio::test]
            async fn should_reject_non_contiguous_append() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                let entry2 = LogEntry {
                    index: 2,
                    term: 1,
                    data: vec![],
                };
                let entry3 = LogEntry {
                    index: 3,
                    term: 1,
                    data: vec![],
                };

                let result = node
                    .reconcile_log(
                        LogIndex::new(1),
                        Term::new(1),
                        vec![entry2, entry3],
                        LogIndex::ZERO,
                    )
                    .unwrap();

                assert!(!result.success);
                assert_eq!(node.last_log_index().unwrap(), LogIndex::ZERO);
            }

            #[tokio::test]
            async fn should_reject_append_when_gap_exists_between_prev_index_and_entries() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                log_store
                    .append_entries(vec![LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    }])
                    .unwrap();
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                let entry3 = LogEntry {
                    index: 3,
                    term: 1,
                    data: vec![],
                };

                let result = node
                    .reconcile_log(LogIndex::new(1), Term::new(1), vec![entry3], LogIndex::ZERO)
                    .unwrap();

                assert!(!result.success);
            }
        }

        mod on_commit_advancement {
            use super::*;

            #[tokio::test]
            async fn should_cap_last_committed_at_local_log_length() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                log_store
                    .append_entries(vec![LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    }])
                    .unwrap();
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                // Scenario: Leader has commit_index 10, but our log only reaches 2 after
                // append.
                let entry2 = LogEntry {
                    index: 2,
                    term: 1,
                    data: vec![],
                };

                let _result = node
                    .reconcile_log(
                        LogIndex::new(1),
                        Term::new(1),
                        vec![entry2],
                        LogIndex::new(10),
                    )
                    .unwrap();

                assert_eq!(node.last_committed(), LogIndex::new(2));
            }
        }
    }

    mod attempt_grant_vote {
        use super::*;

        mod on_existing_vote {
            use super::*;

            #[test]
            fn should_respect_voting_state_when_attempting_to_grant_vote() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();
                node.advance_term(Term::new(1)).unwrap();
                node.persist_vote(NodeId::try_new(3).unwrap()).unwrap();

                let granted = node
                    .attempt_grant_vote(
                        NodeId::try_new(2).unwrap(),
                        Term::new(1),
                        LogIndex::ZERO,
                        Term::ZERO,
                    )
                    .unwrap();
                assert!(!granted);

                let granted = node
                    .attempt_grant_vote(
                        NodeId::try_new(3).unwrap(),
                        Term::new(1),
                        LogIndex::ZERO,
                        Term::ZERO,
                    )
                    .unwrap();
                assert!(granted);
            }
        }

        mod on_log_up_to_date_check {
            use super::*;

            fn setup_node_with_log(
                last_idx: u64,
                last_term: u64,
            ) -> RaftNode<Follower, MockFsm> {
                let fsm = Arc::new(MockFsm::default());
                let log_store = MemoryStorage::new();
                let mut entries = Vec::new();
                for i in 1..last_idx {
                    entries.push(LogEntry {
                        index: i,
                        term: 1,
                        data: vec![],
                    });
                }
                if last_idx > 0 {
                    entries.push(LogEntry {
                        index: last_idx,
                        term: last_term,
                        data: vec![],
                    });
                }
                log_store.append_entries(entries).unwrap();
                RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    Arc::new(log_store),
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap()
            }

            #[test]
            fn should_reject_vote_when_last_term_is_stale() {
                let node = setup_node_with_log(5, 2);
                assert!(
                    !node
                        .is_log_up_to_date(Term::new(1), LogIndex::new(10))
                        .unwrap()
                );
            }

            #[test]
            fn should_accept_vote_when_last_term_is_higher() {
                let node = setup_node_with_log(5, 2);
                assert!(
                    node.is_log_up_to_date(Term::new(3), LogIndex::new(1))
                        .unwrap()
                );
            }

            #[test]
            fn should_handle_index_check_when_terms_are_equal() {
                let node = setup_node_with_log(5, 2);
                assert!(
                    !node
                        .is_log_up_to_date(Term::new(2), LogIndex::new(4))
                        .unwrap()
                );
                assert!(
                    node.is_log_up_to_date(Term::new(2), LogIndex::new(5))
                        .unwrap()
                );
                assert!(
                    node.is_log_up_to_date(Term::new(2), LogIndex::new(6))
                        .unwrap()
                );
            }
        }
    }

    mod try_into_candidate {
        use super::*;

        mod on_election_timeout {
            use super::*;

            #[test]
            fn should_preserve_invariants_when_promoting_to_candidate() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let node = RaftNode::<Follower, MockFsm>::try_new(
                    test_identity(1),
                    fsm,
                    log_store,
                    Tick::new(0),
                    TickDuration::new(100),
                )
                .unwrap();

                let candidate = node
                    .try_into_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap();

                assert_eq!(candidate.current_term().unwrap(), Term::new(1));
                assert_eq!(
                    candidate.voted_for().unwrap(),
                    Some(NodeId::try_new(1).unwrap())
                );
                assert_eq!(candidate.state().vote_count(), 1);
            }
        }
    }
}

mod candidate {
    use super::*;

    mod on_election_restart {
        use super::*;

        #[test]
        fn should_increment_term_and_vote_for_self_on_election_restart() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let node = setup_node_as_candidate(fsm, log_store);
            let initial_term = node.current_term().unwrap();

            let restarted = node
                .try_into_restarted_candidate(Tick::new(0), TickDuration::new(150))
                .unwrap();

            assert_eq!(
                restarted.current_term().unwrap(),
                (initial_term + 1).unwrap()
            );
            assert_eq!(restarted.voted_for().unwrap(), Some(restarted.node_id()));
            assert_eq!(restarted.state().vote_count(), 1);
        }
    }

    mod on_election_victory {
        use super::*;

        #[test]
        fn should_initialize_leader_state_with_next_index_at_end_of_log() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            // Append some entries to the log
            log_store
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

            let node = setup_node_as_candidate(fsm, log_store);
            let peer_ids = vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()];

            let leader = node
                .try_into_leader(peer_ids.clone(), Tick::new(0))
                .unwrap();

            assert_eq!(leader.last_log_index().unwrap(), LogIndex::new(2));
            for peer_id in peer_ids {
                assert_eq!(
                    *leader.state().next_index().get(&peer_id).unwrap(),
                    LogIndex::new(3)
                );
                assert_eq!(
                    *leader.state().match_index().get(&peer_id).unwrap(),
                    LogIndex::ZERO
                );
            }
        }
    }

    mod vote_counting {
        use super::*;

        #[test]
        fn should_be_idempotent_when_adding_vote_per_peer() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = setup_node_as_candidate(fsm, log_store);

            node.state_mut().add_vote(NodeId::try_new(2).unwrap());
            node.state_mut().add_vote(NodeId::try_new(2).unwrap()); // Duplicate

            assert_eq!(node.state().vote_count(), 2); // Self (from setup) + Node 2
        }
    }
}

mod leader {
    use super::*;

    mod propose {
        use super::*;

        #[test]
        fn should_increment_log_length_and_use_current_term() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = setup_node_as_leader(fsm, log_store);
            let current_term = node.current_term().unwrap();

            let index = node.propose(vec![42]).unwrap();

            assert_eq!(index, LogIndex::new(1));
            assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
            let entry = node.read_entries(index, index).unwrap().remove(0);
            assert_eq!(Term::new(entry.term), current_term);
            assert_eq!(entry.data, vec![42]);
        }

        #[test]
        fn should_return_error_when_storage_fails() {
            use common::types::errors::LogStorageError;

            #[derive(Debug, Default)]
            struct FailingAppendStorage;
            impl LogStorage for FailingAppendStorage {
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
                    Ok(())
                }

                fn save_last_committed(&self, _: LogIndex) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn append_entries(&self, _: Vec<LogEntry>) -> Result<(), LogStorageError> {
                    Err(LogStorageError::persistence("Simulated Append Failure"))
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

            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(FailingAppendStorage);
            let mut node = RaftNode {
                identity: test_identity(1),
                fsm,
                log_store,
                last_committed: LogIndex::ZERO,
                last_applied: LogIndex::ZERO,
                state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
            };

            let result = node.propose(vec![1]);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Append Failure"));
        }
    }

    mod heartbeat_epochs {
        use super::*;

        #[test]
        fn should_advance_epoch_when_quorum_is_reached() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = setup_node_as_candidate(fsm, log_store)
                .try_into_leader(
                    vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                    Tick::new(0),
                )
                .unwrap();
            let self_id = node.node_id();

            // 1. Initial state
            assert_eq!(node.state().current_read_epoch(), 0);
            assert_eq!(node.state().confirmed_read_epoch(), 0);

            // 2. Start round 1 (prepare_read_probe increments to 1)
            let target = node.state_mut().prepare_read_probe(self_id);
            assert_eq!(target, 1);

            // 3. Acknowledge from peer 2 (1 peer + self = 2/3 quorum)
            node.state_mut()
                .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
            assert_eq!(node.state().confirmed_read_epoch(), 1);

            // 4. Start round 2 (increments to 2)
            let target2 = node.state_mut().prepare_read_probe(self_id);
            assert_eq!(target2, 2);
            assert_eq!(node.state().confirmed_read_epoch(), 1);

            // 5. Reach quorum for round 2
            node.state_mut()
                .acknowledge_heartbeat(NodeId::try_new(3).unwrap(), 2);
            assert_eq!(node.state().confirmed_read_epoch(), 2);
        }
    }
}
