use std::sync::Arc;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::proto::v1::raft::consensus_service_server::ConsensusService;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info_span;

use crate::node::RaftNodeState;
use crate::service::common::ServiceState;

/// Implementation of the internal Raft consensus RPCs.
///
/// This service acts as a dispatcher, delegating logic to the underlying
/// Type-State node engine while enforcing cluster identity and node health.
#[derive(Debug)]
pub struct ConsensusDispatcher {
    identity: Arc<NodeIdentity>,
    state: Arc<RwLock<RaftNodeState>>,
}

impl ConsensusDispatcher {
    pub fn new(identity: Arc<NodeIdentity>, state: Arc<RwLock<RaftNodeState>>) -> Self {
        Self { identity, state }
    }
}

impl ServiceState for ConsensusDispatcher {
    fn identity_arc(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }
}

#[tonic::async_trait]
impl ConsensusService for ConsensusDispatcher {
    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        let req = request.into_inner();

        let candidate_id = req
            .candidate_id
            .parse::<NodeId>()
            .map_err(|_| self.invalid_node_id_status(&req.candidate_id))?;

        let req_term = Term::new(req.term);
        let req_last_log_index = LogIndex::new(req.last_log_index);
        let req_last_log_term = Term::new(req.last_log_term);

        let span = info_span!("request_vote", term = %req_term, candidate = %candidate_id);
        let _enter = span.enter();

        let mut state_guard = self.state.write().await;
        self.verify_node_integrity(&state_guard)?;

        let (current_term, vote_granted) = state_guard
            .handle_request_vote(
                candidate_id,
                req_term,
                req_last_log_index,
                req_last_log_term,
            )
            .map_err(|_| self.poisoned_status())?;

        Ok(Response::new(RequestVoteResponse::new(
            current_term,
            vote_granted,
        )))
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();

        let leader_id = req
            .leader_id
            .parse::<NodeId>()
            .map_err(|_| self.invalid_node_id_status(&req.leader_id))?;

        let req_term = Term::new(req.term);
        let req_prev_log_index = LogIndex::new(req.prev_log_index);
        let req_prev_log_term = Term::new(req.prev_log_term);
        let req_leader_commit = LogIndex::new(req.leader_commit);

        let span = info_span!("append_entries", term = %req_term, leader = %leader_id);
        let _enter = span.enter();

        let mut state_guard = self.state.write().await;
        self.verify_node_integrity(&state_guard)?;

        let (current_term, success, last_log_index) = state_guard
            .handle_append_entries(
                leader_id,
                req_term,
                req_prev_log_index,
                req_prev_log_term,
                req.entries,
                req_leader_commit,
            )
            .await
            .map_err(|_| self.poisoned_status())?;

        Ok(Response::new(AppendEntriesResponse::new(
            current_term,
            success,
            last_log_index,
        )))
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;

    use super::*;
    use crate::fsm::StateMachine;
    use crate::node::Follower;
    use crate::node::RaftNode;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[tonic::async_trait]
    impl StateMachine for MockFsm {
        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Status> {
            Ok(())
        }
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn mock_dispatcher() -> ConsensusDispatcher {
        let id = mock_identity();
        let fsm = Arc::new(MockFsm::default());
        let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone(), fsm));
        ConsensusDispatcher::new(id, Arc::new(RwLock::new(node)))
    }

    mod integrity_check {
        use super::*;

        #[tokio::test]
        #[should_panic(expected = "CRITICAL: Node is in a poisoned state")]
        async fn panics_when_poisoned() {
            let dispatcher = mock_dispatcher();

            // Force the node into a poisoned state for testing
            {
                let mut state_guard = dispatcher.state.write().await;
                *state_guard = RaftNodeState::Poisoned;
            }

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL: Identity divergence detected")]
        async fn panics_on_identity_mismatch() {
            let id = mock_identity();
            // Create a node with a DIFFERENT identity (different node_id)
            let wrong_id = Arc::new(NodeIdentity::new(id.cluster_id().clone(), NodeId::new(99)));
            let fsm = Arc::new(MockFsm::default());
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(wrong_id, fsm));

            // Use the original ID for the dispatcher but the wrong ID for the node
            let dispatcher = ConsensusDispatcher::new(id, Arc::new(RwLock::new(node)));

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }
    }

    mod request_vote {
        use super::*;

        #[tokio::test]
        async fn grants_vote_when_term_is_higher_and_not_voted() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                term: 1, // Follower starts at term 0, so 1 is higher
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn grants_vote_when_term_is_already_current_and_not_voted() {
            let dispatcher = mock_dispatcher();
            // Pre-initialize to term 1
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(Term::new(1), None));
            }

            let req = Request::new(RequestVoteRequest {
                term: 1, // Same as current term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn rejects_vote_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // First, update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(Term::new(2), None));
            }

            let req = Request::new(RequestVoteRequest {
                term: 1, // Older term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_is_shorter_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 2 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::raft::LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    });
                    node.log_mut().push(common::proto::v1::raft::LogEntry {
                        index: 2,
                        term: 1,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter than local (2)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_has_older_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 2
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::raft::LogEntry {
                        index: 1,
                        term: 2,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 10, // Longer, but...
                last_log_term: 1,   // ...older term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_is_longer_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::raft::LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 2, // Longer than local (1)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_has_newer_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 10 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    for i in 1..=10 {
                        node.log_mut().push(common::proto::v1::raft::LogEntry {
                            index: i as u64,
                            term: 1,
                            data: vec![],
                        });
                    }
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter, but...
                last_log_term: 2,  // ...newer term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
        }
    }

    mod append_entries {
        use std::time::Duration;

        use super::*;

        #[tokio::test]
        async fn returns_success_when_term_is_current() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(AppendEntriesRequest {
                term: 0,
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, true);
            assert_eq!(response.term, 0);
        }

        #[tokio::test]
        async fn rejects_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // Update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(Term::new(2), None));
            }

            let req = Request::new(AppendEntriesRequest {
                term: 1, // Older
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, false);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            // Start as Follower term 0, transition to Candidate term 1
            let follower = RaftNode::<Follower>::new(id.clone(), fsm);
            let candidate = follower.into_candidate();
            let dispatcher = ConsensusDispatcher::new(
                id,
                Arc::new(RwLock::new(RaftNodeState::Candidate(candidate))),
            );

            let req = Request::new(AppendEntriesRequest {
                term: 1, // Equal to candidate term
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, true);

            let state_guard = dispatcher.state.read().await;
            assert!(matches!(&*state_guard, RaftNodeState::Follower(_)));
            assert_eq!(state_guard.current_term().unwrap(), Term::new(1));
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn panics_on_rival_leader_same_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            // Start as Leader term 1
            let follower = RaftNode::<Follower>::new(id.clone(), fsm);
            let candidate = follower.into_candidate();
            let leader = candidate.into_leader(Vec::new());
            let dispatcher =
                ConsensusDispatcher::new(id, Arc::new(RwLock::new(RaftNodeState::Leader(leader))));

            let req = Request::new(AppendEntriesRequest {
                term: 1, // Rival leader for same term
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            // This should panic
            let _ = dispatcher.append_entries(req).await;
        }

        #[tokio::test]
        async fn resets_election_timer() {
            let dispatcher = mock_dispatcher();

            // 1. Get initial heartbeat time
            let initial_heartbeat = {
                let guard = dispatcher.state.read().await;
                if let RaftNodeState::Follower(node) = &*guard {
                    node.state().last_heartbeat()
                } else {
                    panic!("Should be follower");
                }
            };

            // Small sleep to ensure time moves forward
            tokio::time::sleep(Duration::from_millis(5)).await;

            let req = Request::new(AppendEntriesRequest {
                term: 0,
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            dispatcher.append_entries(req).await.unwrap();

            // 2. Verify heartbeat time was updated
            let updated_heartbeat = {
                let guard = dispatcher.state.read().await;
                if let RaftNodeState::Follower(node) = &*guard {
                    node.state().last_heartbeat()
                } else {
                    panic!("Should be follower");
                }
            };

            assert!(updated_heartbeat > initial_heartbeat);
        }
    }
}
