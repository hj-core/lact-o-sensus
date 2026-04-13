use std::sync::Arc;

use common::proto::v1::AppendEntriesRequest;
use common::proto::v1::AppendEntriesResponse;
use common::proto::v1::RequestVoteRequest;
use common::proto::v1::RequestVoteResponse;
use common::proto::v1::consensus_service_server::ConsensusService;
use common::types::NodeId;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;

use crate::identity::NodeIdentity;
use crate::node::Follower;
use crate::node::RaftNode;
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
    fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    fn state(&self) -> &Arc<RwLock<RaftNodeState>> {
        &self.state
    }
}

#[tonic::async_trait]
impl ConsensusService for ConsensusDispatcher {
    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        let req = request.into_inner();
        self.verify_cluster_id(&req.cluster_id)?;

        let mut state_guard = self.state.write().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("request_vote", term = req.term, candidate = %req.candidate_id);
        let _enter = span.enter();

        let candidate_id = req
            .candidate_id
            .parse::<NodeId>()
            .map_err(|_| self.invalid_node_id_status(&req.candidate_id))?;

        // 1. If term > currentTerm: set currentTerm = term, transition to follower
        //    (§5.1)
        let current_term = state_guard
            .current_term()
            .map_err(|_| self.poisoned_status())?;

        if req.term > current_term {
            info!(
                "Received higher term ({}) from candidate {}. Transitioning to Follower.",
                req.term, req.candidate_id
            );
            state_guard.transition(|old| old.into_follower(req.term, None));
        }

        // Re-acquire current state after potential transition
        let mut vote_granted = false;
        match &mut *state_guard {
            RaftNodeState::Follower(node) => {
                // 2. If votedFor is null or candidateId, and candidate’s log is at
                // least as up-to-date as receiver’s log, grant vote (§5.2, §5.4)
                if req.term >= node.current_term()
                    && (node.voted_for().is_none() || node.voted_for() == Some(candidate_id))
                {
                    // TODO: Phase 5 - Add log up-to-date check (§5.4)
                    vote_granted = true;
                    node.vote_for(candidate_id);
                    info!(
                        "Granting vote to candidate {} for term {}",
                        req.candidate_id, req.term
                    );
                }
            }
            RaftNodeState::Candidate(_) | RaftNodeState::Leader(_) => {
                // If term is equal, we already voted for ourselves or are
                // leading. If term was higher, we transitioned
                // to Follower above.
            }
            RaftNodeState::Poisoned => return Err(self.poisoned_status()),
        };

        Ok(Response::new(RequestVoteResponse {
            cluster_id: self.cluster_id_as_str().to_string(),
            term: state_guard
                .current_term()
                .map_err(|_| self.poisoned_status())?,
            vote_granted,
        }))
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();
        self.verify_cluster_id(&req.cluster_id)?;

        let mut state_guard = self.state.write().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("append_entries", term = req.term, leader = %req.leader_id);
        let _enter = span.enter();

        let current_term = state_guard
            .current_term()
            .map_err(|_| self.poisoned_status())?;

        // 1. Reply false if term < currentTerm (§5.1)
        if req.term < current_term {
            debug!(
                "Rejecting AppendEntries from {}: term {} is older than currentTerm {}",
                req.leader_id, req.term, current_term
            );
            return Ok(Response::new(AppendEntriesResponse {
                cluster_id: self.cluster_id_as_str().to_string(),
                term: current_term,
                success: false,
                last_log_index: 0,
            }));
        }

        // 2. Term-based state transitions
        let leader_id = req.leader_id.parse::<NodeId>().ok();

        if req.term > current_term {
            // §5.1: If term > currentTerm, transition to follower
            info!(
                "Received higher term ({}) from leader {}. Demoting to Follower.",
                req.term, req.leader_id
            );
            state_guard.transition(|old| old.into_follower(req.term, leader_id));
        } else if req.term == current_term {
            match &*state_guard {
                RaftNodeState::Candidate(_) => {
                    // §5.2: If Candidate receives AppendEntries from a leader of the SAME term,
                    // it recognizes the leader as legitimate and returns to follower state.
                    info!(
                        "Candidate recognizing leader {} for term {}. Returning to Follower.",
                        req.leader_id, req.term
                    );
                    state_guard.transition(|old| old.into_follower(req.term, leader_id));
                }
                RaftNodeState::Leader(_) => {
                    // FATAL INVARIANT VIOLATION: Two leaders for the same term!
                    // We prioritize Safety over Liveness. Detecting another leader for our own term
                    // implies a failure in the consensus logic or persistence layer. We must halt
                    // to prevent data corruption.
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. Halting \
                         node to prevent state corruption.",
                        req.leader_id, req.term
                    );
                    error!("{}", msg);
                    panic!("{}", msg);
                }
                _ => {} // Followers just accept heartbeats from their leader
            }
        }

        // 3. Reset election timer to acknowledge the leader's presence
        state_guard.reset_heartbeat();

        // Note: Real log consistency checks (prev_log_index/term) and log replication
        // will be implemented in Phase 5. For Phase 3, we acknowledge the heartbeat.

        Ok(Response::new(AppendEntriesResponse {
            cluster_id: self.cluster_id_as_str().to_string(),
            term: req.term,
            success: true,
            last_log_index: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;

    use super::*;

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn mock_dispatcher() -> ConsensusDispatcher {
        let id = mock_identity();
        let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
        ConsensusDispatcher::new(id, Arc::new(RwLock::new(node)))
    }

    mod identity_guard {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_cluster_id_mismatches() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                cluster_id: "wrong-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    mod health_check {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_poisoned() {
            let dispatcher = mock_dispatcher();

            // Force the node into a poisoned state for testing
            {
                let mut state_guard = dispatcher.state.write().await;
                *state_guard = RaftNodeState::Poisoned;
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }
    }

    mod request_vote {
        use super::*;

        #[tokio::test]
        async fn grants_vote_when_term_is_current_and_not_voted() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Follower starts at term 0, so 1 is higher/current
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
                state.transition(|old| old.into_follower(2, None));
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Older term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
            assert_eq!(response.term, 2);
        }
    }

    mod append_entries {
        use std::time::Duration;

        use super::*;

        #[tokio::test]
        async fn returns_success_when_term_is_current() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
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
                state.transition(|old| old.into_follower(2, None));
            }

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
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
            // Start as Follower term 0, transition to Candidate term 1
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = follower.into_candidate();
            let dispatcher = ConsensusDispatcher::new(
                id,
                Arc::new(RwLock::new(RaftNodeState::Candidate(candidate))),
            );

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
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
            assert_eq!(state_guard.current_term().unwrap(), 1);
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn panics_on_rival_leader_same_term() {
            let id = mock_identity();
            // Start as Leader term 1
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = follower.into_candidate();
            let leader = candidate.into_leader();
            let dispatcher =
                ConsensusDispatcher::new(id, Arc::new(RwLock::new(RaftNodeState::Leader(leader))));

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
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
                cluster_id: "test-cluster".to_string(),
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
