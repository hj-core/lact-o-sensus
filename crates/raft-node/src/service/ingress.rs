use std::sync::Arc;

use common::proto::v1::MutationStatus;
use common::proto::v1::ProposeMutationRequest;
use common::proto::v1::ProposeMutationResponse;
use common::proto::v1::QueryStateRequest;
use common::proto::v1::QueryStateResponse;
use common::proto::v1::ingress_service_server::IngressService;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::info_span;

use crate::identity::NodeIdentity;
use crate::node::RaftNodeState;
use crate::service::common::ServiceState;

/// Implementation of the external client ingress RPCs.
///
/// This service handles user mutations and state queries, enforcing
/// cluster identity and redirecting clients to the current leader.
pub struct IngressDispatcher {
    identity: Arc<NodeIdentity>,
    state: Arc<RwLock<RaftNodeState>>,
}

impl IngressDispatcher {
    pub fn new(identity: Arc<NodeIdentity>, state: Arc<RwLock<RaftNodeState>>) -> Self {
        Self { identity, state }
    }
}

impl ServiceState for IngressDispatcher {
    fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    fn state(&self) -> &Arc<RwLock<RaftNodeState>> {
        &self.state
    }
}

#[tonic::async_trait]
impl IngressService for IngressDispatcher {
    async fn propose_mutation(
        &self,
        request: Request<ProposeMutationRequest>,
    ) -> Result<Response<ProposeMutationResponse>, Status> {
        let req = request.into_inner();
        self.verify_cluster_id(&req.cluster_id)?;

        let state_guard = self.state.read().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("propose_mutation", client = %req.client_id, seq = req.sequence_id);
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(_) => {
                // ADR 002: Clients must communicate only with the Leader for mutations.
                info!("Rejecting mutation: Node is in Follower state.");
                // TODO: Phase 4 - Provide actual leader_hint for redirection
                Ok(Response::new(ProposeMutationResponse {
                    cluster_id: self.identity.cluster_id.clone(),
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: "Node is a Follower. Please retry with the Leader.".to_string(),
                }))
            }
            RaftNodeState::Poisoned => unreachable!("Caught by check_state_health"),
            _ => {
                // TODO: Phase 4 - Implement Leader proposal logic
                Ok(Response::new(ProposeMutationResponse {
                    cluster_id: self.identity.cluster_id.clone(),
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: "Node state not yet capable of mutations.".to_string(),
                }))
            }
        }
    }

    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let req = request.into_inner();
        self.verify_cluster_id(&req.cluster_id)?;

        let state_guard = self.state.read().await;
        self.check_state_health(&state_guard)?;

        let span = info_span!("query_state");
        let _enter = span.enter();

        // TODO: Phase 5 - Implement State Machine queries
        Ok(Response::new(QueryStateResponse {
            cluster_id: self.identity.cluster_id.clone(),
            items: Vec::new(),
            current_state_version: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Follower;
    use crate::node::RaftNode;

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity {
            cluster_id: "test-cluster".to_string(),
            node_id: 1,
        })
    }

    fn mock_dispatcher() -> IngressDispatcher {
        let id = mock_identity();
        let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
        IngressDispatcher::new(id, Arc::new(RwLock::new(node)))
    }

    mod identity_guard {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_cluster_id_mismatches() {
            let dispatcher = mock_dispatcher();
            let req = Request::new(ProposeMutationRequest {
                cluster_id: "wrong-cluster".to_string(),
                ..Default::default()
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    mod propose_mutation {
        use super::*;

        #[tokio::test]
        async fn returns_rejected_when_follower() {
            let dispatcher = mock_dispatcher();
            let req = Request::new(ProposeMutationRequest {
                cluster_id: "test-cluster".to_string(),
                client_id: "user-1".to_string(),
                sequence_id: 1,
                ..Default::default()
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.error_message.contains("Follower"));
        }
    }
}
