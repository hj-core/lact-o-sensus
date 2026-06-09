use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::InstallSnapshotRequest;
use common::proto::v1::raft::InstallSnapshotResponse;
use common::proto::v1::raft::LogEntry;
use common::proto::v1::raft::PreVoteRequest;
use common::proto::v1::raft::PreVoteResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::proto::v1::raft::consensus_service_server::ConsensusService;
use common::proto::v1::raft::consensus_service_server::ConsensusServiceServer;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::trace::TraceId;
use common_rpc::HEADER_TRACE_ID;
use futures::FutureExt;
use futures::stream;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tokio::sync::oneshot;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::async_trait;
use tonic::transport::Server;

use super::election::initiate_election;
use super::election::process_vote_response;
use super::lifecycle::should_compact_log;
use super::replication::broadcast_append_entries;
use super::replication::process_replication_response;
use super::replication::replicate_snapshot_to_peer;
use super::rpc::determine_replication_strategy;
use super::rpc::update_leader_last_committed;
use super::rpc::verify_trace_integrity;
use super::*;
use crate::config::Config;
use crate::engine::LogicalNode;
use crate::engine::RoleState;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;
use crate::storage::LogStorage;
use crate::storage::MemoryStorage;
use crate::tick::TickDuration;
use crate::tick::TickThresholds;

struct TestContext<S: StateMachine> {
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    mock_peer: Option<MockPeerHandle>,
}

struct MockPeerHandle {
    pub shutdown_tx: Option<oneshot::Sender<()>>,
    pub service: Arc<MockConsensusService>,
}

impl Drop for MockPeerHandle {
    /// Clinical RAII Trigger: Sends a non-blocking shutdown signal to the
    /// mock server during panic or scope exit (ADR 009).
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl<S: StateMachine> TestContext<S> {
    /// Initializes a new test context fixture.
    ///
    /// If `with_remote_peer` is true, spawns a background gRPC mock server
    /// to simulate a remote node in the cluster topology.
    async fn setup_with_fsm(fsm: Arc<S>, with_remote_peer: bool) -> Self {
        let config = mock_config(50, 100);
        let id = Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(1).unwrap(),
        ));
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = StdRng::seed_from_u64(1);
        let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
        let state = Arc::new(ConsensusShell::new(node));

        let mut peer_manager = Arc::new(PeerManager::try_new(id.clone(), &HashMap::new()).unwrap());
        let mut mock_peer = None;

        if with_remote_peer {
            let service = Arc::new(MockConsensusService {
                vote_response: Arc::new(Mutex::new(RequestVoteResponse::new(Term::ZERO, true))),
                append_response: Arc::new(Mutex::new(AppendEntriesResponse::new(
                    Term::ZERO,
                    true,
                    LogIndex::ZERO,
                ))),
                snapshot_response: Arc::new(Mutex::new(InstallSnapshotResponse::new(Term::ZERO))),
                pre_vote_response: Arc::new(Mutex::new(PreVoteResponse::new(Term::ZERO, true))),
            });

            let (tx, rx) = oneshot::channel::<()>();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound_addr = listener.local_addr().unwrap();

            let service_clone = service.clone();
            tokio::spawn(async move {
                let incoming = stream::unfold(listener, |listener| async move {
                    let res = listener.accept().await.map(|(s, _)| s);
                    Some((res, listener))
                });

                Server::builder()
                    .add_service(ConsensusServiceServer::from_arc(service_clone))
                    .serve_with_incoming_shutdown(incoming, rx.map(|_| ()))
                    .await
                    .expect("Mock server failed");
            });

            let mut peer_map = HashMap::new();
            let peer_id = NodeId::try_new(2).unwrap();
            peer_map.insert(peer_id, format!("http://{}", bound_addr));
            peer_manager = Arc::new(PeerManager::try_new(id, &peer_map).unwrap());

            mock_peer = Some(MockPeerHandle {
                shutdown_tx: Some(tx),
                service,
            });
        }

        TestContext {
            config,
            state,
            peer_manager,
            mock_peer,
        }
    }
}

impl TestContext<MockFsm> {
    /// Specialized helper for standard consensus tests.
    async fn setup(with_remote_peer: bool) -> Self {
        Self::setup_with_fsm(Arc::new(MockFsm), with_remote_peer).await
    }
}

struct MockConsensusService {
    vote_response: Arc<Mutex<RequestVoteResponse>>,
    append_response: Arc<Mutex<AppendEntriesResponse>>,
    snapshot_response: Arc<Mutex<InstallSnapshotResponse>>,
    pre_vote_response: Arc<Mutex<PreVoteResponse>>,
}

#[async_trait]
impl ConsensusService for MockConsensusService {
    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
        let mut res = Response::new(*self.vote_response.lock().unwrap());
        if let Some(val) = trace_id_header {
            res.metadata_mut().insert(HEADER_TRACE_ID, val);
        }
        Ok(res)
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
        let mut res = Response::new(*self.append_response.lock().unwrap());
        if let Some(val) = trace_id_header {
            res.metadata_mut().insert(HEADER_TRACE_ID, val);
        }
        Ok(res)
    }

    async fn install_snapshot(
        &self,
        request: Request<InstallSnapshotRequest>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
        let mut res = Response::new(*self.snapshot_response.lock().unwrap());
        if let Some(val) = trace_id_header {
            res.metadata_mut().insert(HEADER_TRACE_ID, val);
        }
        Ok(res)
    }

    async fn pre_vote(
        &self,
        request: Request<PreVoteRequest>,
    ) -> Result<Response<PreVoteResponse>, Status> {
        let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
        let mut res = Response::new(*self.pre_vote_response.lock().unwrap());
        if let Some(val) = trace_id_header {
            res.metadata_mut().insert(HEADER_TRACE_ID, val);
        }
        Ok(res)
    }
}

#[derive(Debug, Default)]
struct MockFsm;
use common::types::errors::FsmError;
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
        _last_included_index: LogIndex,
        _data: &[u8],
        _trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn mock_config(min_ms: u64, max_ms: u64) -> Arc<Config> {
    let toml_str = format!(
        r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            peers = {{}}
            [raft]
            election_timeout_min_ms = {}
            election_timeout_max_ms = {}
            snapshot_threshold = 20
            [policy]
            veto_addr = "http://127.0.0.1:50060"
            veto_timeout_ms = 1000
        "#,
        min_ms, max_ms
    );
    Arc::new(toml::from_str(&toml_str).unwrap())
}

mod initiate_election {
    use super::*;

    mod campaign_lifecycle {
        use super::*;

        #[tokio::test]
        async fn should_transition_to_leader_when_quorum_reached() {
            let ctx = TestContext::setup(true).await;
            {
                let node_id = ctx.state.read().await.identity().node_id();

                // 1. Setup Candidate state
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                }

                let params = ElectionCampaignParams {
                    term: Term::new(1),
                    node_id,
                    last_log_index: LogIndex::ZERO,
                    last_log_term: Term::ZERO,
                    trace_id: TraceId::generate(),
                };

                // 4. Run election
                initiate_election(
                    ctx.config.clone(),
                    ctx.state.clone(),
                    ctx.peer_manager.clone(),
                    params,
                )
                .await
                .expect("Failed to run election in test");

                // 5. Verify transition
                {
                    let guard = ctx.state.read().await;
                    assert!(matches!(guard.state(), RoleState::Leader(_)));
                }
            }
        }
    }

    mod when_storage_fails_during_term_check {
        use super::*;
        use crate::test_utils::FailingTermStorage;

        #[tokio::test]
        async fn returns_error_propagated_to_start_election_campaign() {
            let config = mock_config(50, 100);
            let id = Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::try_new(1).unwrap(),
            ));
            let storage = Arc::new(FailingTermStorage::with_succeed_count(1));
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let node =
                LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                    .unwrap();
            let state = Arc::new(ConsensusShell::new(node));
            let peer_manager = Arc::new(PeerManager::try_new(id, &HashMap::new()).unwrap());
            let params = ElectionCampaignParams {
                term: Term::new(1),
                node_id: NodeId::try_new(1).unwrap(),
                last_log_index: LogIndex::ZERO,
                last_log_term: Term::ZERO,
                trace_id: TraceId::generate(),
            };

            let result = initiate_election(config, state, peer_manager, params).await;

            assert!(result.is_err(), "Expected Err from storage failure");
        }
    }
}

mod process_vote_response {
    use super::*;

    mod when_storage_fails_during_tally {
        use super::*;
        use crate::test_utils::FailingTermStorage;

        #[tokio::test]
        async fn triggers_halt_mandate() {
            let _config = mock_config(50, 100);
            let id = Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::try_new(1).unwrap(),
            ));
            // Keep a handle to control when failure triggers
            let failing = Arc::new(FailingTermStorage::with_succeed_count(10));
            let storage: Arc<dyn LogStorage> = failing.clone();
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let node =
                LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                    .unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            // Transition to candidate (so as_candidate_mut returns Some)
            {
                let mut guard = state.write().await;
                guard.into_candidate();
            }

            // Arm the failure: the next current_term() call will fail
            failing.set_succeed_count(0);

            let peer_ids = vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()];
            let res = Ok(RequestVoteResponse::new(Term::new(1), true));

            let state_clone = state.clone();
            let handle = tokio::spawn(async move {
                process_vote_response(
                    &state_clone,
                    Term::new(1),
                    &peer_ids,
                    NodeId::try_new(2).unwrap(),
                    res,
                )
                .await
            });

            let result = handle.await;
            assert!(result.is_err(), "Expected panic (Halt Mandate)");

            // Verify node is poisoned after the panic
            let guard = state.read().await;
            assert!(matches!(guard.state(), RoleState::Poisoned));
        }
    }

    mod discovering_higher_term {
        use super::*;
        #[tokio::test]
        async fn should_demote_to_follower_when_peer_has_newer_term() {
            let ctx = TestContext::setup(false).await;
            {
                let res = Ok(RequestVoteResponse::new(Term::new(2), false));
                let action = process_vote_response(
                    &ctx.state,
                    Term::new(1),
                    &ctx.peer_manager.peer_ids(),
                    NodeId::try_new(2).unwrap(),
                    res,
                )
                .await
                .unwrap();
                assert_eq!(action, VoteAction::Demoted);
                assert_eq!(
                    ctx.state.read().await.try_current_term().unwrap(),
                    Term::new(2)
                );
            }
        }
    }

    mod reaching_quorum {
        use super::*;
        #[tokio::test]
        async fn should_transition_to_leader_when_majority_votes_granted() {
            let ctx = TestContext::setup(false).await;
            {
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                }
                let res = Ok(RequestVoteResponse::new(Term::new(1), true));
                let action = process_vote_response(
                    &ctx.state,
                    Term::new(1),
                    &[NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                    NodeId::try_new(2).unwrap(),
                    res,
                )
                .await
                .unwrap();
                assert_eq!(action, VoteAction::QuorumReached);
                assert!(matches!(
                    ctx.state.read().await.state(),
                    RoleState::Leader(_)
                ));
            }
        }
    }
}

mod replicate_to_peers {
    use super::*;

    mod broadcast_lifecycle {
        use super::*;

        #[tokio::test]
        async fn should_fan_out_to_all_peers_when_invoked() {
            let ctx = TestContext::setup(false).await;
            {
                let p1 = NodeId::try_new(2).unwrap();
                let p2 = NodeId::try_new(3).unwrap();

                let mut peer_map = HashMap::new();
                peer_map.insert(p1, "http://127.0.0.1:50091".to_string());
                peer_map.insert(p2, "http://127.0.0.1:50092".to_string());

                let pm = Arc::new(
                    PeerManager::try_new(ctx.state.read().await.identity(), &peer_map).unwrap(),
                );

                let params = ReplicationRoundParams {
                    term: Term::new(1),
                    node_id: NodeId::try_new(1).unwrap(),
                    last_committed: LogIndex::ZERO,
                    trace_id: TraceId::generate(),
                };

                let stream =
                    broadcast_append_entries(ctx.config.as_ref(), pm, ctx.state.clone(), params);
                assert_eq!(stream.len(), 2);
            }
        }
    }
}

mod process_replication_response {
    use super::*;

    mod when_storage_fails_during_term_check {
        use super::*;
        use crate::test_utils::FailingTermStorage;

        #[tokio::test]
        async fn triggers_halt_mandate() {
            let id = Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::try_new(1).unwrap(),
            ));
            let failing = Arc::new(FailingTermStorage::with_succeed_count(10));
            let storage: Arc<dyn LogStorage> = failing.clone();
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let node =
                LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                    .unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            // Transition to leader (so as_leader_mut returns Some)
            let peer_id = NodeId::try_new(2).unwrap();
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![peer_id]);
            }

            // Arm the failure: the next current_term() call will fail
            failing.set_succeed_count(0);

            let res = Ok(Some(ReplicationOutcome::AppendEntries {
                sent_prev_index: LogIndex::new(0),
                sent_entries_len: 1,
                response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
            }));

            let state_clone = state.clone();
            let handle = tokio::spawn(async move {
                process_replication_response(&state_clone, Term::new(1), peer_id, res).await
            });

            let result = handle.await;
            assert!(result.is_err(), "Expected panic (Halt Mandate)");

            let guard = state.read().await;
            assert!(matches!(guard.state(), RoleState::Poisoned));
        }
    }

    mod successful_replication {
        use super::*;
        #[tokio::test]
        async fn should_advance_indices_when_peer_accepts_entries() {
            let ctx = TestContext::setup(false).await;
            {
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                }
                let res = Ok(Some(ReplicationOutcome::AppendEntries {
                    sent_prev_index: LogIndex::new(0),
                    sent_entries_len: 1,
                    response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
                }));
                process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                    .await
                    .expect("Failed to advance horizon in test");
                let guard = ctx.state.read().await;
                if let RoleState::Leader(node) = guard.state() {
                    assert_eq!(
                        *node.state().match_index().get(&peer_id).unwrap(),
                        LogIndex::new(1)
                    );
                    assert_eq!(
                        *node.state().next_index().get(&peer_id).unwrap(),
                        LogIndex::new(2)
                    );
                } else {
                    panic!("Should be leader");
                }
            }
        }
    }

    mod log_mismatch_handling {
        use super::*;
        #[tokio::test]
        async fn should_optimize_backoff_when_peer_rejects_due_to_mismatch() {
            let ctx = TestContext::setup(false).await;
            {
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                    if let Some(node) = guard.as_leader_mut() {
                        node.state_mut()
                            .next_index_mut()
                            .insert(peer_id, LogIndex::new(11));
                    }
                }
                let res = Ok(Some(ReplicationOutcome::AppendEntries {
                    sent_prev_index: LogIndex::new(10),
                    sent_entries_len: 0,
                    response: AppendEntriesResponse::new(Term::new(1), false, LogIndex::new(5)),
                }));
                process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                    .await
                    .expect("Failed to advance horizon in test");
                let guard = ctx.state.read().await;
                if let RoleState::Leader(node) = guard.state() {
                    assert_eq!(
                        *node.state().next_index().get(&peer_id).unwrap(),
                        LogIndex::new(6)
                    );
                } else {
                    panic!("Should be leader");
                }
            }
        }
    }

    mod successful_snapshot_installation {
        use super::*;
        #[tokio::test]
        async fn should_advance_indices_to_snapshot_horizon_when_installation_succeeds() {
            let ctx = TestContext::setup(false).await;
            {
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                }

                let snapshot_index = LogIndex::new(50);
                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let res = Ok(Some(ReplicationOutcome::InstallSnapshot {
                    last_included_index: snapshot_index,
                    response: InstallSnapshotResponse::new(Term::new(1)),
                    _permit: permit,
                }));

                process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                    .await
                    .expect("Failed to advance horizon in test");

                let guard = ctx.state.read().await;
                if let RoleState::Leader(node) = guard.state() {
                    assert_eq!(
                        *node.state().match_index().get(&peer_id).unwrap(),
                        snapshot_index
                    );
                    assert_eq!(
                        *node.state().next_index().get(&peer_id).unwrap(),
                        LogIndex::new(51)
                    );
                } else {
                    panic!("Should be leader");
                }
            }
        }
    }
}

mod replicate_snapshot_to_peer {
    use super::*;

    mod higher_term_discovery {
        use super::*;
        #[tokio::test]
        async fn should_abort_and_return_outcome_when_probe_discovers_higher_term() {
            let ctx = TestContext::setup(true).await;
            {
                let mock = ctx.mock_peer.as_ref().unwrap();

                // Simulate higher term on peer
                let higher_term = Term::new(10);
                *mock.service.append_response.lock().unwrap() =
                    AppendEntriesResponse::new(higher_term, false, LogIndex::ZERO);

                let last_included_index = LogIndex::new(10);
                let last_included_term = Term::new(2);
                let peer_id = NodeId::try_new(2).unwrap();
                let params = ReplicationRoundParams {
                    term: Term::new(3), // Current term is 3, peer is 10
                    node_id: NodeId::try_new(1).unwrap(),
                    last_committed: LogIndex::new(0),
                    trace_id: TraceId::generate(),
                };

                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let res = replicate_snapshot_to_peer(
                    ctx.state.clone(),
                    ctx.peer_manager.clone(),
                    peer_id,
                    params,
                    last_included_index,
                    last_included_term,
                    Duration::from_secs(1),
                    Duration::from_secs(30),
                    permit,
                )
                .await;

                // Verify: Returns Ok(Some(Outcome)) with the higher term
                assert!(res.is_ok());
                let outcome = res.unwrap().expect("Should return outcome");
                if let ReplicationOutcome::AppendEntries { response, .. } = outcome {
                    assert_eq!(Term::new(response.term), higher_term);
                } else {
                    panic!("Expected AppendEntries outcome from probe");
                }

                // Verify: FSM serialization was never triggered (flag is false)
                assert!(!ctx.state.is_frozen());
            }
        }
    }

    mod normal_operation {
        use super::*;
        #[tokio::test]
        async fn should_proceed_when_probe_is_successful_with_current_term() {
            let ctx = TestContext::setup(true).await;
            {
                let last_included_index = LogIndex::new(10);
                let last_included_term = Term::new(2);
                let peer_id = NodeId::try_new(2).unwrap();
                let params = ReplicationRoundParams {
                    term: Term::new(3),
                    node_id: NodeId::try_new(1).unwrap(),
                    last_committed: LogIndex::new(0),
                    trace_id: TraceId::generate(),
                };

                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let res = replicate_snapshot_to_peer(
                    ctx.state.clone(),
                    ctx.peer_manager.clone(),
                    peer_id,
                    params,
                    last_included_index,
                    last_included_term,
                    Duration::from_secs(1),
                    Duration::from_secs(30),
                    permit,
                )
                .await;

                // Verify: Successfully completed Phase 1 and Phase 2
                assert!(res.is_ok());
                assert!(res.unwrap().is_some());
            }
        }

        #[tokio::test]
        async fn should_toggle_freeze_flag_during_serialization() {
            use std::sync::atomic::AtomicBool;
            use std::sync::atomic::Ordering;

            use tokio::sync::Mutex as TokioMutex;

            #[derive(Debug)]
            struct ObservantFsm {
                shell: Arc<TokioMutex<Option<Arc<ConsensusShell<ObservantFsm>>>>>,
                flag_during_snapshot: Arc<AtomicBool>,
            }

            impl StateMachine for ObservantFsm {
                type Error = FsmError;

                fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                    Ok(LogIndex::ZERO)
                }

                fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                    Ok(())
                }

                fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                    if let Some(shell) = self.shell.blocking_lock().as_ref() {
                        let flag = shell.is_frozen();
                        self.flag_during_snapshot.store(flag, Ordering::SeqCst);
                    }
                    Ok(vec![])
                }

                fn install_snapshot(
                    &self,
                    _idx: LogIndex,
                    _data: &[u8],
                    _tid: TraceId,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            let flag_during_snapshot = Arc::new(AtomicBool::new(false));
            let fsm = Arc::new(ObservantFsm {
                shell: Arc::new(TokioMutex::new(None)),
                flag_during_snapshot: flag_during_snapshot.clone(),
            });

            let ctx = TestContext::setup_with_fsm(fsm.clone(), true).await;
            fsm.shell.lock().await.replace(ctx.state.clone());

            {
                let last_included_index = LogIndex::new(10);
                let last_included_term = Term::new(2);
                let peer_id = NodeId::try_new(2).unwrap();
                let params = ReplicationRoundParams {
                    term: Term::new(3),
                    node_id: NodeId::try_new(1).unwrap(),
                    last_committed: LogIndex::new(0),
                    trace_id: TraceId::generate(),
                };

                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let _ = replicate_snapshot_to_peer(
                    ctx.state.clone(),
                    ctx.peer_manager.clone(),
                    peer_id,
                    params,
                    last_included_index,
                    last_included_term,
                    Duration::from_secs(1),
                    Duration::from_secs(30),
                    permit,
                )
                .await;

                assert!(
                    flag_during_snapshot.load(Ordering::SeqCst),
                    "Flag should be true during snapshot()"
                );
                assert!(
                    !ctx.state.is_frozen(),
                    "Flag should be false after snapshot completes"
                );
            }
        }
    }
}

mod prepare_and_replicate_to_peer {
    use super::*;

    mod failure_handling {
        use super::*;
        #[tokio::test]
        #[should_panic(expected = "Snapshot serialization failed for peer=2 at index=10 in \
                                   term=3: Persistence failure: Simulated failure")]
        async fn should_apply_fatal_with_rich_forensic_context_when_snapshot_fails() {
            #[derive(Debug)]
            struct FailingFsm;
            impl StateMachine for FailingFsm {
                type Error = FsmError;

                fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                    Ok(LogIndex::ZERO)
                }

                fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                    Ok(())
                }

                fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                    Err(FsmError::persistence("Simulated failure"))
                }

                fn install_snapshot(
                    &self,
                    _idx: LogIndex,
                    _data: &[u8],
                    _tid: TraceId,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            let fsm = Arc::new(FailingFsm);
            let ctx = TestContext::setup_with_fsm(fsm, true).await;

            {
                let last_included_index = LogIndex::new(10);
                let last_included_term = Term::new(2);
                let peer_id = NodeId::try_new(2).unwrap();
                let params = ReplicationRoundParams {
                    term: Term::new(3),
                    node_id: NodeId::try_new(1).unwrap(),
                    last_committed: LogIndex::new(0),
                    trace_id: TraceId::generate(),
                };

                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let _ = replicate_snapshot_to_peer(
                    ctx.state.clone(),
                    ctx.peer_manager.clone(),
                    peer_id,
                    params,
                    last_included_index,
                    last_included_term,
                    Duration::from_secs(1),
                    Duration::from_secs(30),
                    permit,
                )
                .await;
            }
        }
    }
}

mod update_leader_last_committed {
    use super::*;

    mod quorum_commitment {
        use super::*;
        #[tokio::test]
        async fn should_advance_commit_index_when_majority_matches_index() {
            let ctx = TestContext::setup(false).await;
            {
                let p2 = NodeId::try_new(2).unwrap();
                let p3 = NodeId::try_new(3).unwrap();
                {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![p2, p3]);
                    if let Some(leader) = guard.as_leader_mut() {
                        let entries: Vec<_> = (1..=5)
                            .map(|i| LogEntry {
                                index: i,
                                term: 1,
                                data: vec![],
                            })
                            .collect();
                        leader.log_store().append_entries(entries).unwrap();
                    }
                }
                {
                    let mut guard = ctx.state.write().await;
                    if let Some(node) = guard.as_leader_mut() {
                        node.state_mut()
                            .match_index_mut()
                            .insert(p2, LogIndex::new(4));
                        node.state_mut()
                            .match_index_mut()
                            .insert(p3, LogIndex::new(1));
                    }
                    update_leader_last_committed(&mut guard);
                    assert_eq!(guard.last_committed(), LogIndex::new(4));
                }
            }
        }
    }
}

mod verify_trace_integrity {
    use common_rpc::TraceInterceptor;
    use tonic::Response;

    use super::*;

    mod matching_traces {
        use super::*;
        #[test]
        fn should_pass_when_returned_trace_matches_expected() {
            let trace_id = TraceId::generate();
            let mut response = Response::new(());
            TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)
                .expect("Should inject trace ID");
            assert!(
                verify_trace_integrity(&response, trace_id, NodeId::try_new(2).unwrap()).is_ok()
            );
        }
    }

    mod mismatched_traces {
        use super::*;
        #[test]
        fn should_fail_with_data_loss_when_returned_trace_differs() {
            let expected = TraceId::generate();
            let got = TraceId::generate();
            let mut response = Response::new(());
            TraceInterceptor::inject_trace_id_into_response(&mut response, got)
                .expect("Should inject trace ID");
            let res = verify_trace_integrity(&response, expected, NodeId::try_new(2).unwrap());
            assert!(res.is_err());
            assert_eq!(res.unwrap_err().code(), tonic::Code::DataLoss);
        }
    }

    mod missing_traces {
        use super::*;
        #[test]
        fn should_fail_with_data_loss_when_trace_id_is_absent() {
            let expected = TraceId::generate();
            let response = Response::new(());
            let res = verify_trace_integrity(&response, expected, NodeId::try_new(2).unwrap());
            assert!(res.is_err());
            assert_eq!(res.unwrap_err().code(), tonic::Code::DataLoss);
        }
    }
}

mod leader_replication_fallback {
    use super::*;

    mod lagging_follower {
        use super::*;

        #[tokio::test]
        async fn should_fallback_to_install_snapshot_when_next_index_is_behind_horizon() {
            let ctx = TestContext::setup(false).await;
            {
                let peer_id = NodeId::try_new(2).unwrap();

                // 1. Setup Leader with a compacted log
                let params = {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);

                    // Compact log up to index 10
                    guard.save_snapshot_metadata(LogIndex::new(10), Term::new(1));

                    // Set follower's next_index to 5 (behind the 10 horizon)
                    if let Some(leader) = guard.as_leader_mut() {
                        leader
                            .state_mut()
                            .next_index_mut()
                            .insert(peer_id, LogIndex::new(5));
                    }

                    ReplicationRoundParams {
                        term: Term::new(1),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    }
                };

                // 2. Verify intent identification logic (Locked phase)
                let mut guard = ctx.state.write().await;
                if let RoleState::Leader(_) = guard.state() {
                    let strategy = determine_replication_strategy(&mut guard, peer_id, params)
                        .unwrap()
                        .expect("Failed to advance horizon in test");

                    assert!(matches!(
                        strategy,
                        ReplicationStrategy::InstallSnapshot { .. }
                    ));

                    if let ReplicationStrategy::InstallSnapshot {
                        last_included_index,
                        ..
                    } = strategy
                    {
                        assert_eq!(last_included_index, LogIndex::new(10));
                    }
                } else {
                    panic!("Should be leader");
                }
            }
        }
    }
}

mod should_compact_log {
    use super::*;

    fn append_dummy_entries<S: StateMachine>(guard: &mut LogicalNode<S>, count: u64) {
        let entries: Vec<_> = (1..=count)
            .map(|i| LogEntry {
                index: i,
                term: 1,
                data: vec![],
            })
            .collect();
        guard.log_store().append_entries(entries).unwrap();
    }

    mod triggers {
        use super::*;
        #[tokio::test]
        async fn should_trigger_compaction_when_applied_entries_exceed_threshold() {
            let ctx = TestContext::setup(false).await;
            {
                let mut guard = ctx.state.write().await;

                // Threshold is 20 in mock_config.
                append_dummy_entries(&mut guard, 21);

                // Advance applied index forward to trigger compaction
                guard
                    .advance_horizon_after_snapshot(LogIndex::new(21))
                    .expect("Failed to advance horizon in test");

                assert!(should_compact_log(&mut guard, &ctx.config, false));
            }
        }

        #[tokio::test]
        async fn should_not_trigger_compaction_when_applied_index_is_below_threshold() {
            let ctx = TestContext::setup(false).await;
            {
                let mut guard = ctx.state.write().await;

                append_dummy_entries(&mut guard, 5);

                // applied = 5, last_included = 0. log_length = 5 <= 20.
                guard
                    .advance_horizon_after_snapshot(LogIndex::new(5))
                    .expect("Failed to advance horizon in test");

                assert!(!should_compact_log(&mut guard, &ctx.config, false));
            }
        }

        #[tokio::test]
        async fn should_not_trigger_compaction_when_log_is_long_but_applied_is_low() {
            let ctx = TestContext::setup(false).await;
            {
                let mut guard = ctx.state.write().await;

                // log_index = 100, applied = 5. threshold = 20.
                // Under previous logic (log_index based), this would trigger.
                // Under new logic (applied based), it should NOT trigger.
                append_dummy_entries(&mut guard, 100);
                guard
                    .advance_horizon_after_snapshot(LogIndex::new(5))
                    .expect("Failed to advance horizon in test");

                assert!(!should_compact_log(&mut guard, &ctx.config, false));
            }
        }

        #[tokio::test]
        async fn should_inhibit_compaction_when_snapshot_is_in_progress() {
            let ctx = TestContext::setup(false).await;
            {
                let mut guard = ctx.state.write().await;

                append_dummy_entries(&mut guard, 25);

                guard
                    .advance_horizon_after_snapshot(LogIndex::new(25))
                    .expect("Failed to advance horizon in test");
                ctx.state.freeze().unwrap();

                assert!(!should_compact_log(&mut guard, &ctx.config, true));
            }
        }
    }
}

mod reachability_first_snapshotting {
    use super::*;

    mod probe_lifecycle {
        use super::*;

        #[tokio::test]
        async fn should_abort_snapshot_when_reachability_probe_fails() {
            let ctx = TestContext::setup(false).await;
            {
                let peer_id = NodeId::try_new(2).unwrap();

                // 1. Setup Leader with lagging peer
                let params = {
                    let mut guard = ctx.state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                    guard.save_snapshot_metadata(LogIndex::new(10), Term::new(1));
                    if let Some(leader) = guard.as_leader_mut() {
                        leader
                            .state_mut()
                            .next_index_mut()
                            .insert(peer_id, LogIndex::new(5));
                    }
                    ReplicationRoundParams {
                        term: Term::new(1),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    }
                };

                // 2. Configure PeerManager with a dead-end address (will trigger hang/error)
                let mut peer_map = HashMap::new();
                peer_map.insert(peer_id, "http://127.0.0.1:1".to_string()); // Invalid port
                let pm = Arc::new(
                    PeerManager::try_new(ctx.state.read().await.identity(), &peer_map).unwrap(),
                );

                // 3. Execute snapshot task
                let permit = ctx.state.try_acquire_snapshot_permit(peer_id).unwrap();
                let res = replicate_snapshot_to_peer(
                    ctx.state.clone(),
                    pm,
                    peer_id,
                    params,
                    LogIndex::new(10),
                    Term::new(1),
                    Duration::from_millis(10), // Short timeout
                    Duration::from_secs(30),   // Long snapshot timeout
                    permit,
                )
                .await;

                // 4. Verify: No error returned, but no snapshot outcome either
                assert!(res.is_ok());
                assert!(
                    res.unwrap().is_none(),
                    "Should have aborted before heavy work"
                );

                // 5. Verify: FSM was never frozen
                assert!(!ctx.state.is_frozen());
            }
        }
    }

    mod spawn_background_applier {
        use super::*;

        #[derive(Debug)]
        struct PoisonApplyFsm;

        impl StateMachine for PoisonApplyFsm {
            type Error = FsmError;

            fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                Ok(LogIndex::ZERO)
            }

            fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                Err(FsmError::invariant("Simulated FSM apply failure"))
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

        #[tokio::test]
        async fn should_poison_node_when_fsm_apply_fails_via_background_applier() {
            let fsm = Arc::new(PoisonApplyFsm);
            let id = Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::try_new(1).unwrap(),
            ));
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let node =
                LogicalNode::try_new(id.clone(), fsm, storage.clone(), thresholds, rng).unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            // Append and commit an entry so apply_committed has work to do.
            storage
                .append_entries(vec![common::proto::v1::raft::LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![1],
                }])
                .unwrap();

            {
                let mut guard = state.write().await;
                guard.advance_last_committed(LogIndex::new(1));
            }

            // Spawn apply_committed in a separate task to catch the panic
            // from apply_fatal.
            let state_clone = state.clone();
            let handle = tokio::spawn(async move {
                crate::orchestration::apply_committed(&state_clone).await;
            });

            let result = handle.await;
            assert!(
                result.is_err(),
                "Expected apply_committed to panic on FSM failure"
            );

            // Node should be poisoned after the fatal error.
            let guard = state.read().await;
            assert!(guard.is_poisoned());
        }
    }

    // =========================================================================
    // Phase 8: Pre-Vote Integrity (Election Safety)
    // =========================================================================
    mod pre_vote {
        use super::*;
        use crate::consensus::election::initiate_election;
        use crate::consensus::election::initiate_pre_vote;
        use crate::consensus::election::start_pre_vote_campaign;
        use crate::consensus::types::ElectionCampaignParams;
        use crate::consensus::types::PreVoteCampaignParams;
        use crate::engine::TickAction;

        mod campaign_lifecycle {
            use super::*;

            #[tokio::test]
            async fn partitioned_node_does_not_disrupt_cluster_term() {
                // Setup: Node 1 is Leader at term 5 with committed entries.
                // Node 2 (the partitioned one) has term 3 with stale log.
                let config = mock_config(50, 100);
                let id_leader = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage_leader = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let leader_fsm = Arc::new(MockFsm);
                let leader_node = LogicalNode::try_new(
                    id_leader.clone(),
                    leader_fsm,
                    storage_leader,
                    thresholds,
                    rng,
                )
                .unwrap();
                let leader_state = Arc::new(ConsensusShell::new(leader_node));

                // Peer manager with Node 2 as remote peer
                let mut peer_map = HashMap::new();
                let service = Arc::new(MockConsensusService {
                    vote_response: Arc::new(Mutex::new(RequestVoteResponse::new(
                        Term::new(5),
                        true,
                    ))),
                    append_response: Arc::new(Mutex::new(AppendEntriesResponse::new(
                        Term::new(5),
                        true,
                        LogIndex::new(10),
                    ))),
                    snapshot_response: Arc::new(Mutex::new(InstallSnapshotResponse::new(
                        Term::new(5),
                    ))),
                    pre_vote_response: Arc::new(Mutex::new(PreVoteResponse::new(
                        Term::new(5),
                        false,
                    ))),
                });
                let (tx, rx) = oneshot::channel::<()>();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let bound_addr = listener.local_addr().unwrap();
                let service_clone = service.clone();
                tokio::spawn(async move {
                    let incoming = stream::unfold(listener, |listener| async move {
                        let res = listener.accept().await.map(|(s, _)| s);
                        Some((res, listener))
                    });
                    Server::builder()
                        .add_service(ConsensusServiceServer::from_arc(service_clone))
                        .serve_with_incoming_shutdown(incoming, rx.map(|_| ()))
                        .await
                        .expect("Mock server failed");
                });
                let peer_id = NodeId::try_new(2).unwrap();
                peer_map.insert(peer_id, format!("http://{}", bound_addr));
                let pm = Arc::new(PeerManager::try_new(id_leader.clone(), &peer_map).unwrap());

                // Set leader: into_candidate() increments term, so leader ends at term 6
                {
                    let mut guard = leader_state.write().await;
                    guard.into_follower(Term::new(5), None);
                    guard.into_candidate();
                    guard.into_leader(pm.peer_ids());
                }
                let leader_term = Term::new(6);

                // Now simulate the partitioned node (Node 2) trying to start a pre-vote
                // Node 2 is at term 3 with stale log
                let partitioned_params = PreVoteCampaignParams {
                    term: Term::new(3),
                    node_id: peer_id,
                    last_log_index: LogIndex::new(1),
                    last_log_term: Term::new(3),
                    rpc_timeout: config.raft.rpc_timeout(),
                    trace_id: TraceId::generate(),
                };

                // Execute pre-vote campaign
                start_pre_vote_campaign(
                    config.clone(),
                    leader_state.clone(),
                    pm.clone(),
                    partitioned_params,
                    tracing::Span::current(),
                );

                // Give the campaign time to complete
                tokio::time::sleep(Duration::from_millis(200)).await;

                // Assert: Cluster term is unchanged (not disrupted by partitioned node)
                let guard = leader_state.read().await;
                assert_eq!(
                    guard.try_current_term().unwrap(),
                    leader_term,
                    "Cluster term must remain stable during asymmetrical network partitions"
                );

                // Assert: Leader is still Leader (not demoted by pre-vote)
                assert!(
                    matches!(guard.state(), RoleState::Leader(_)),
                    "Existing leader must not be demoted by a stale pre-vote request"
                );

                let _ = tx.send(());
            }

            #[tokio::test]
            async fn pre_vote_quorum_triggers_real_election() {
                // Single-node cluster: self-vote gives quorum immediately
                // (1 total node, quorum = 1, pre_votes_granted starts at 1).
                let config = mock_config(50, 100);
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let fsm = Arc::new(MockFsm);
                let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                let pm = Arc::new(PeerManager::try_new(id.clone(), &HashMap::new()).unwrap());

                // Pre-vote campaign: current term is 0
                let params = PreVoteCampaignParams {
                    term: Term::ZERO,
                    node_id: id.node_id(),
                    last_log_index: LogIndex::ZERO,
                    last_log_term: Term::ZERO,
                    rpc_timeout: config.raft.rpc_timeout(),
                    trace_id: TraceId::generate(),
                };

                // Need to be in PreCandidate state for campaign to work
                {
                    let mut guard = state.write().await;
                    guard.into_pre_candidate();
                }

                // Single-node: quorum reached immediately via self-vote
                initiate_pre_vote(config.clone(), state.clone(), pm.clone(), params)
                    .await
                    .expect("Pre-vote campaign should succeed");

                // Verify pre-vote campaign transitioned to Candidate
                {
                    let guard = state.read().await;
                    assert!(
                        matches!(guard.state(), RoleState::Candidate(_)),
                        "Pre-vote campaign should have transitioned to Candidate"
                    );
                    assert_eq!(
                        guard.try_current_term().unwrap(),
                        Term::new(1),
                        "Candidate term should be 1"
                    );
                }

                // Now trigger the real election (the tick loop would normally
                // dispatch this via StartElection from the Candidate's evaluate_tick).
                let current_term = state.read().await.try_current_term().unwrap();
                let election_params = ElectionCampaignParams {
                    term: current_term,
                    node_id: id.node_id(),
                    last_log_index: LogIndex::ZERO,
                    last_log_term: Term::ZERO,
                    trace_id: TraceId::generate(),
                };
                initiate_election(config, state.clone(), pm.clone(), election_params)
                    .await
                    .expect("Real election should succeed after pre-vote quorum");

                // Assert: Node is now Leader at term 1
                let guard = state.read().await;
                assert!(
                    matches!(guard.state(), RoleState::Leader(_)),
                    "Node should become Leader after pre-vote quorum and real election"
                );
                assert_eq!(
                    guard.try_current_term().unwrap(),
                    Term::new(1),
                    "Term should be 1"
                );
            }

            #[tokio::test]
            async fn pre_vote_no_quorum_returns_to_follower_without_term_change() {
                let config = mock_config(50, 100);
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let fsm = Arc::new(MockFsm);
                let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Peer that denies pre-vote
                let mut peer_map = HashMap::new();
                let service = Arc::new(MockConsensusService {
                    vote_response: Arc::new(Mutex::new(RequestVoteResponse::new(
                        Term::ZERO,
                        false,
                    ))),
                    append_response: Arc::new(Mutex::new(AppendEntriesResponse::new(
                        Term::ZERO,
                        false,
                        LogIndex::ZERO,
                    ))),
                    snapshot_response: Arc::new(Mutex::new(InstallSnapshotResponse::new(
                        Term::ZERO,
                    ))),
                    pre_vote_response: Arc::new(Mutex::new(PreVoteResponse::new(
                        Term::ZERO,
                        false,
                    ))),
                });
                let (tx, rx) = oneshot::channel::<()>();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let bound_addr = listener.local_addr().unwrap();
                let service_clone = service.clone();
                tokio::spawn(async move {
                    let incoming = stream::unfold(listener, |listener| async move {
                        let res = listener.accept().await.map(|(s, _)| s);
                        Some((res, listener))
                    });
                    Server::builder()
                        .add_service(ConsensusServiceServer::from_arc(service_clone))
                        .serve_with_incoming_shutdown(incoming, rx.map(|_| ()))
                        .await
                        .expect("Mock server failed");
                });
                let peer_id = NodeId::try_new(2).unwrap();
                peer_map.insert(peer_id, format!("http://{}", bound_addr));
                let pm = Arc::new(PeerManager::try_new(id.clone(), &peer_map).unwrap());

                // Pre-vote campaign at the current term (0).
                let params = PreVoteCampaignParams {
                    term: Term::ZERO,
                    node_id: id.node_id(),
                    last_log_index: LogIndex::ZERO,
                    last_log_term: Term::ZERO,
                    rpc_timeout: config.raft.rpc_timeout(),
                    trace_id: TraceId::generate(),
                };

                // Transition to PreCandidate
                {
                    let mut guard = state.write().await;
                    guard.into_pre_candidate();
                }

                start_pre_vote_campaign(
                    config,
                    state.clone(),
                    pm.clone(),
                    params,
                    tracing::Span::current(),
                );

                // Wait for campaign to finish, then tick to trigger StepDown
                tokio::time::sleep(Duration::from_millis(200)).await;
                // Tick the node to trigger the pre-vote campaign timeout
                for _ in 0..10 {
                    let mut guard = state.write().await;
                    if let RoleState::PreCandidate(_) = guard.state()
                        && guard.tick() == TickAction::StepDown
                    {
                        guard.into_follower(Term::ZERO, None);
                        break;
                    }
                }

                // Assert: Back to Follower with term unchanged
                let guard = state.read().await;
                assert!(
                    matches!(guard.state(), RoleState::Follower(_)),
                    "Node should return to Follower after failed pre-vote"
                );
                assert_eq!(
                    guard.try_current_term().unwrap(),
                    Term::ZERO,
                    "Term must remain unchanged after failed pre-vote"
                );

                let _ = tx.send(());
            }

            #[tokio::test]
            async fn stale_candidate_rejected() {
                // Node at term 5. A stale candidate at term 2 should be
                // rejected by grant_pre_vote's new term fence.
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let fsm = Arc::new(MockFsm);
                let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Set node's term to 5
                {
                    let mut guard = state.write().await;
                    guard.into_follower(Term::new(5), None);
                }

                // Stale candidate at term 2 should be rejected
                let result = {
                    let mut guard = state.write().await;
                    guard.handle_pre_vote(
                        NodeId::try_new(2).unwrap(),
                        Term::new(2),
                        LogIndex::ZERO,
                        Term::ZERO,
                    )
                };

                assert!(
                    !result.vote_granted,
                    "Stale candidate must not get pre-vote"
                );
                assert_eq!(
                    result.term,
                    Term::new(5),
                    "Response term must reflect node's current term"
                );
            }

            #[tokio::test]
            async fn up_to_date_candidate_accepted() {
                // Node at term 3. A candidate at the same term with an
                // up-to-date log should be granted the pre-vote.
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let fsm = Arc::new(MockFsm);
                let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Set node's term to 3
                {
                    let mut guard = state.write().await;
                    guard.into_follower(Term::new(3), None);
                }

                // Candidate at same term 3 with up-to-date log should be
                // granted pre-vote
                let result = {
                    let mut guard = state.write().await;
                    guard.handle_pre_vote(
                        NodeId::try_new(2).unwrap(),
                        Term::new(3),
                        LogIndex::ZERO,
                        Term::ZERO,
                    )
                };

                assert!(
                    result.vote_granted,
                    "Up-to-date candidate should get pre-vote"
                );
                assert_eq!(
                    result.term,
                    Term::new(3),
                    "Response term must match node's current term"
                );
            }
        }
    }
}
