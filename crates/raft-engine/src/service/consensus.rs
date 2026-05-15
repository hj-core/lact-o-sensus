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
use common::types::errors::NodeError;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info_span;

use crate::engine::LogicalNode;
use crate::shell::ConsensusShell;

/// Implementation of the internal Raft consensus RPCs.
///
/// This service acts as a dispatcher, delegating logic to the underlying
/// Type-State node engine while enforcing cluster identity and node health.
#[derive(Debug)]
pub struct ConsensusDispatcher {
    identity: Arc<NodeIdentity>,
    state: Arc<ConsensusShell>,
}

impl ConsensusDispatcher {
    pub fn new(identity: Arc<NodeIdentity>, state: Arc<ConsensusShell>) -> Self {
        Self { identity, state }
    }

    /// Verifies that the node engine is healthy and matches the service
    /// identity.
    fn verify_node_integrity(&self, node: &mut LogicalNode) -> Result<(), Status> {
        let engine_id = node.identity();
        if engine_id == &*self.identity {
            Ok(())
        } else {
            let msg = format!(
                "Identity divergence detected! ServiceIdentity='{:?}' EngineIdentity='{:?}'",
                self.identity, engine_id
            );
            // ADR 009: Poison the node via apply_fatal before panicking.
            node.apply_fatal(NodeError::Identity(msg));
        }
    }

    /// Returns a standard gRPC InvalidArgument status for invalid Node IDs.
    fn invalid_node_id_status(&self, input: &str) -> Status {
        Status::invalid_argument(format!("Invalid NodeId format: '{}'", input))
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

        let result = {
            let mut guard = self.state.write().await;
            self.verify_node_integrity(&mut guard)?;

            guard.handle_request_vote(
                candidate_id,
                req_term,
                req_last_log_index,
                req_last_log_term,
            )
        };

        Ok(Response::new(RequestVoteResponse::new(
            result.term,
            result.vote_granted,
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

        let result = {
            let mut guard = self.state.write().await;
            self.verify_node_integrity(&mut guard)?;

            guard
                .handle_append_entries(
                    leader_id,
                    req_term,
                    req_prev_log_index,
                    req_prev_log_term,
                    req.entries,
                    req_leader_commit,
                )
                .await
        };

        Ok(Response::new(AppendEntriesResponse::new(
            result.term,
            result.success,
            result.conflict_index,
        )))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use common::raft_api::StateMachine;
    use common::types::ClusterId;
    use common::types::errors::FsmError;

    use super::*;
    use crate::engine::Follower;
    use crate::node::RaftNode;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            Ok(())
        }
    }

    fn test_identity(id: u64) -> NodeIdentity {
        NodeIdentity::new(ClusterId::try_new("test-cluster").unwrap(), NodeId::new(id))
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(test_identity(1))
    }

    fn mock_dispatcher() -> ConsensusDispatcher {
        let id = mock_identity();
        let fsm = Arc::new(MockFsm);
        let storage = Box::new(MemoryStorage::new());
        let node = LogicalNode::Follower(RaftNode::<Follower>::new((*id).clone(), fsm, storage));
        let state = Arc::new(ConsensusShell::new(node));
        ConsensusDispatcher::new(id, state)
    }

    mod integrity_check {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;

        use super::*;

        #[tokio::test]
        #[should_panic(expected = "Halt Mandate: Node is poisoned")]
        async fn panics_when_poisoned() {
            let dispatcher = mock_dispatcher();

            // Force the node into a poisoned state for testing
            {
                let mut guard = dispatcher.state.write().await;
                *guard = LogicalNode::Poisoned;
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
        #[should_panic(expected = "Identity Integrity Violation: Identity divergence detected")]
        async fn panics_on_node_id_mismatch() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Box::new(MemoryStorage::new());
            // Different NodeId, same ClusterId
            let node =
                LogicalNode::Follower(RaftNode::<Follower>::new(test_identity(99), fsm, storage));
            let state = Arc::new(ConsensusShell::new(node));

            let dispatcher = ConsensusDispatcher::new(id, state);

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        #[should_panic(expected = "Identity Integrity Violation: Identity divergence detected")]
        async fn panics_on_cluster_id_mismatch() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Box::new(MemoryStorage::new());
            // Same NodeId, different ClusterId
            let cluster_mismatch =
                NodeIdentity::new(ClusterId::try_new("wrong-cluster").unwrap(), id.node_id());
            let node =
                LogicalNode::Follower(RaftNode::<Follower>::new(cluster_mismatch, fsm, storage));
            let state = Arc::new(ConsensusShell::new(node));

            let dispatcher = ConsensusDispatcher::new(id, state);

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        async fn identity_mismatch_poisons_node() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Box::new(MemoryStorage::new());
            let node =
                LogicalNode::Follower(RaftNode::<Follower>::new(test_identity(99), fsm, storage));
            let state = Arc::new(ConsensusShell::new(node));
            let dispatcher = ConsensusDispatcher::new(id, state.clone());

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            // Catch the panic to inspect the state afterwards
            let result = AssertUnwindSafe(dispatcher.request_vote(req))
                .catch_unwind()
                .await;

            assert!(result.is_err());

            // Verify that the node is now permanently poisoned
            let guard = state.read().await;
            assert!(guard.is_poisoned());
        }
    }

    mod request_vote {
        use common::proto::v1::raft::LogEntry;

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
            assert!(response.vote_granted);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn grants_vote_when_term_is_already_current_and_not_voted() {
            let dispatcher = mock_dispatcher();
            // Pre-initialize to term 1
            {
                let mut state = dispatcher.state.write().await;
                state.into_follower(Term::new(1), None);
            }

            let req = Request::new(RequestVoteRequest {
                term: 1, // Same as current term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(response.vote_granted);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn rejects_vote_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // First, update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.into_follower(Term::new(2), None);
            }

            let req = Request::new(RequestVoteRequest {
                term: 1, // Older term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(!response.vote_granted);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_is_shorter_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 2 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let LogicalNode::Follower(node) = &mut *state {
                    node.storage_mut()
                        .append_entries(vec![
                            LogEntry::new(LogIndex::new(1), Term::new(1), vec![]),
                            LogEntry::new(LogIndex::new(2), Term::new(1), vec![]),
                        ])
                        .unwrap();
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter than local (2)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(!response.vote_granted);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_has_older_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 2
            {
                let mut state = dispatcher.state.write().await;
                if let LogicalNode::Follower(node) = &mut *state {
                    node.storage_mut()
                        .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(2), vec![])])
                        .unwrap();
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 10, // Longer, but...
                last_log_term: 1,   // ...older term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(!response.vote_granted);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_is_longer_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let LogicalNode::Follower(node) = &mut *state {
                    node.storage_mut()
                        .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![])])
                        .unwrap();
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 2, // Longer than local (1)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(response.vote_granted);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_has_newer_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 10 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let LogicalNode::Follower(node) = &mut *state {
                    let mut entries = Vec::new();
                    for i in 1..=10 {
                        entries.push(LogEntry::new(LogIndex::new(i as u64), Term::new(1), vec![]));
                    }
                    node.storage_mut().append_entries(entries).unwrap();
                }
            }

            let req = Request::new(RequestVoteRequest {
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter, but...
                last_log_term: 2,  // ...newer term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert!(response.vote_granted);
        }
    }

    mod append_entries {
        use std::time::Duration;

        use super::*;
        use crate::storage::MemoryStorage;

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
            assert!(response.success);
            assert_eq!(response.term, 0);
        }

        #[tokio::test]
        async fn rejects_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // Update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.into_follower(Term::new(2), None);
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
            assert!(!response.success);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Box::new(MemoryStorage::new());
            // Start as Follower term 0, transition to Candidate term 1
            let follower = RaftNode::<Follower>::new((*id).clone(), fsm, storage);
            let candidate = follower.into_candidate().unwrap();
            let state = Arc::new(ConsensusShell::new(LogicalNode::Candidate(candidate)));
            let dispatcher = ConsensusDispatcher::new(id, state);

            let req = Request::new(AppendEntriesRequest {
                term: 1, // Equal to candidate term
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert!(response.success);

            let mut state_guard = dispatcher.state.write().await;
            assert!(matches!(&*state_guard, LogicalNode::Follower(_)));
            assert_eq!(state_guard.current_term(), Term::new(1));
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn panics_on_rival_leader_same_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Box::new(MemoryStorage::new());
            // Start as Leader term 1
            let follower = RaftNode::<Follower>::new((*id).clone(), fsm, storage);
            let candidate = follower.into_candidate().unwrap();
            let leader = candidate.into_leader(Vec::new()).unwrap();
            let state = Arc::new(ConsensusShell::new(LogicalNode::Leader(leader)));
            let dispatcher = ConsensusDispatcher::new(id, state);

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
                if let LogicalNode::Follower(node) = &*guard {
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
                if let LogicalNode::Follower(node) = &*guard {
                    node.state().last_heartbeat()
                } else {
                    panic!("Should be follower");
                }
            };

            assert!(updated_heartbeat > initial_heartbeat);
        }
    }
}
