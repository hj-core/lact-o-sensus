use std::sync::Arc;

use common::proto::v1::AppendEntriesRequest;
use common::proto::v1::AppendEntriesResponse;
use common::proto::v1::RequestVoteRequest;
use common::proto::v1::RequestVoteResponse;
use common::proto::v1::consensus_service_server::ConsensusService;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
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

        let state_guard = self.state.read().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("request_vote", term = req.term, candidate = %req.candidate_id);
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(node) => {
                info!(
                    "Received RequestVote as Follower (term: {})",
                    node.current_term()
                );
                // TODO: Phase 3 - Actual voting logic
                Ok(Response::new(RequestVoteResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    term: node.current_term(),
                    vote_granted: false,
                }))
            }
            RaftNodeState::Poisoned => unreachable!("Caught by check_state_health"),
            _ => {
                // Candidates and Leaders also need to handle votes, but for the
                // skeleton we return a basic rejection.
                Ok(Response::new(RequestVoteResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    term: 0,
                    vote_granted: false,
                }))
            }
        }
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();
        self.verify_cluster_id(&req.cluster_id)?;

        let state_guard = self.state.read().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("append_entries", term = req.term, leader = %req.leader_id);
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(node) => {
                debug!(
                    "Received heartbeat as Follower (term: {})",
                    node.current_term()
                );
                // TODO: Phase 3 - Log replication logic
                Ok(Response::new(AppendEntriesResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    term: node.current_term(),
                    success: false,
                    last_log_index: 0,
                }))
            }
            RaftNodeState::Poisoned => unreachable!("Caught by check_state_health"),
            _ => Ok(Response::new(AppendEntriesResponse {
                cluster_id: self.cluster_id_as_str().to_string(),
                term: 0,
                success: false,
                last_log_index: 0,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;

    use super::*;

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            1.into(),
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
        async fn returns_skeletal_reject_when_follower() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
            assert_eq!(response.cluster_id, "test-cluster");
        }
    }

    mod append_entries {
        use super::*;

        #[tokio::test]
        async fn returns_skeletal_failure_when_follower() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, false);
            assert_eq!(response.cluster_id, "test-cluster");
        }
    }
}
