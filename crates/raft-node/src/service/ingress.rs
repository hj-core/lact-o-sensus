use std::sync::Arc;
use std::time::Duration;

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
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;

use crate::identity::NodeIdentity;
use crate::node::Follower;
use crate::node::RaftNode;
use crate::node::RaftNodeState;
use crate::peer::PeerManager;
use crate::service::common::ServiceState;
use crate::service::veto::VetoError;
use crate::service::veto::VetoRelay;

/// Implementation of the external client ingress RPCs.
///
/// This service handles user mutations and state queries, enforcing
/// cluster identity and redirecting clients to the current leader.
#[derive(Debug)]
pub struct IngressDispatcher {
    identity: Arc<NodeIdentity>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
    veto_relay: Arc<dyn VetoRelay>,
    veto_timeout: Duration,
}

impl IngressDispatcher {
    pub fn new(
        identity: Arc<NodeIdentity>,
        state: Arc<RwLock<RaftNodeState>>,
        peer_manager: Arc<PeerManager>,
        veto_relay: Arc<dyn VetoRelay>,
        veto_timeout: Duration,
    ) -> Self {
        Self {
            identity,
            state,
            peer_manager,
            veto_relay,
            veto_timeout,
        }
    }

    /// Helper to generate a standardized leader hint and error message for
    /// external clients when this node is in the Follower state.
    fn redirection_hint(&self, node: &RaftNode<Follower>) -> (String, String) {
        let leader_id = node.state().leader_id();
        match leader_id {
            Some(id) => match self.peer_manager.get_address(id) {
                Ok(addr) => (
                    addr,
                    format!(
                        "Node is a Follower. Please retry with the Leader at NodeID {}.",
                        id
                    ),
                ),
                Err(_) => (
                    String::new(),
                    format!(
                        "Node is a Follower of NodeID {}, but its network address is missing from \
                         our configuration.",
                        id
                    ),
                ),
            },
            None => (
                String::new(),
                "Node is a Follower, but the current leader is unknown. Please retry shortly."
                    .to_string(),
            ),
        }
    }
}

impl ServiceState for IngressDispatcher {
    fn identity_arc(&self) -> &Arc<NodeIdentity> {
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
        self.verify_identity(&req.cluster_id)?;

        let state_guard = self.state.read().await;
        self.verify_node_integrity(&state_guard)?;

        let span = info_span!("propose_mutation", client = %req.client_id, seq = req.sequence_id);
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(node) => {
                let (leader_hint, error_message) = self.redirection_hint(node);
                info!(
                    "Rejecting mutation: Node is a Follower. Redirecting to leader_hint='{}'",
                    leader_hint
                );

                Ok(Response::new(ProposeMutationResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint,
                    error_message,
                }))
            }
            RaftNodeState::Candidate(_) => {
                info!("Rejecting mutation: Node is a Candidate (Election in progress).");
                Ok(Response::new(ProposeMutationResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: "Election in progress. No leader established. Please retry \
                                    shortly."
                        .to_string(),
                }))
            }
            RaftNodeState::Leader(_) => {
                info!("Leader received proposal. Triggering AI Veto evaluation...");

                let outcome = self
                    .veto_relay
                    .evaluate(
                        req.cluster_id.clone(),
                        req.client_id.clone(),
                        req.intent.as_ref().unwrap_or(&Default::default()),
                        &[], // Inventory store implemented in Phase 5
                        self.veto_timeout,
                    )
                    .await;

                match outcome {
                    Ok(veto) => {
                        // ADR 004: Centralized identity verification of the AI response.
                        self.verify_identity(&veto.cluster_id)?;

                        info!(
                            "AI Veto response received: approved={}, justification='{}'",
                            veto.is_approved, veto.moral_justification
                        );

                        // TODO: Phase 4 Step 3 - Handle commit logic
                        Ok(Response::new(ProposeMutationResponse {
                            cluster_id: self.cluster_id_as_str().to_string(),
                            status: MutationStatus::Rejected as i32, // Placeholder
                            state_version: 0,
                            leader_hint: String::new(),
                            error_message: format!(
                                "AI evaluation successful (Approved: {}). Replication not yet \
                                 implemented.",
                                veto.is_approved
                            ),
                        }))
                    }
                    Err(VetoError::Timeout(d)) => {
                        // High-signal warning for transient latency.
                        warn!("AI Veto evaluation timed out after {:?}", d);
                        Ok(Response::new(ProposeMutationResponse {
                            cluster_id: self.cluster_id_as_str().to_string(),
                            status: MutationStatus::Rejected as i32,
                            state_version: 0,
                            leader_hint: String::new(),
                            error_message: "Evaluation timed out. Please retry shortly."
                                .to_string(),
                        }))
                    }
                    Err(VetoError::RpcFailure(e)) => {
                        // Error level for actual infrastructure outages.
                        error!("AI Veto evaluation infrastructure failure: {}", e);
                        Ok(Response::new(ProposeMutationResponse {
                            cluster_id: self.cluster_id_as_str().to_string(),
                            status: MutationStatus::Rejected as i32,
                            state_version: 0,
                            leader_hint: String::new(),
                            error_message: "Internal system error occurred during evaluation."
                                .to_string(),
                        }))
                    }
                }
            }
            RaftNodeState::Poisoned => unreachable!("Caught by verify_health"),
        }
    }

    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let req = request.into_inner();
        self.verify_identity(&req.cluster_id)?;

        let state_guard = self.state.read().await;
        self.verify_node_integrity(&state_guard)?;

        let span = info_span!("query_state");
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(node) => {
                let (leader_hint, _) = self.redirection_hint(node);
                // For queries, we return the leader hint but may eventually support
                // stale reads from followers in Phase 6.
                info!(
                    "Redirecting query: Node is a Follower. Hint='{}'",
                    leader_hint
                );

                // In Phase 4, we don't return an error here, but we could return the hint
                // in metadata or a specific field if the proto allowed it.
                // For now, we return empty results as a placeholder.
                Ok(Response::new(QueryStateResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    items: Vec::new(),
                    current_state_version: 0,
                }))
            }
            _ => {
                // TODO: Phase 5 - Implement State Machine queries
                Ok(Response::new(QueryStateResponse {
                    cluster_id: self.cluster_id_as_str().to_string(),
                    items: Vec::new(),
                    current_state_version: 0,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use common::types::ClusterId;
    use common::types::NodeId;

    use super::*;
    use crate::node::Follower;
    use crate::node::RaftNode;
    use crate::service::veto::MockVetoRelay;

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn mock_peer_manager(peers: &HashMap<NodeId, std::net::SocketAddr>) -> Arc<PeerManager> {
        Arc::new(PeerManager::new(mock_identity(), peers, Duration::from_millis(40)).unwrap())
    }

    fn mock_dispatcher(state: RaftNodeState, peer_manager: Arc<PeerManager>) -> IngressDispatcher {
        IngressDispatcher::new(
            mock_identity(),
            Arc::new(RwLock::new(state)),
            peer_manager,
            Arc::new(MockVetoRelay),
            Duration::from_secs(1),
        )
    }

    mod identity_guard {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_cluster_id_mismatches() {
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(mock_identity()));
            let dispatcher = mock_dispatcher(node, mock_peer_manager(&HashMap::new()));
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
        async fn returns_rejected_when_follower_leader_unknown() {
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(mock_identity()));
            let dispatcher = mock_dispatcher(node, mock_peer_manager(&HashMap::new()));
            let req = Request::new(ProposeMutationRequest {
                cluster_id: "test-cluster".to_string(),
                client_id: "user-1".to_string(),
                sequence_id: 1,
                ..Default::default()
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.error_message.contains("leader is unknown"));
            assert!(response.leader_hint.is_empty());
        }

        #[tokio::test]
        async fn returns_hint_when_follower_leader_known() {
            let mut peers = HashMap::new();
            let leader_addr = "127.0.0.1:50052";
            peers.insert(NodeId::new(2), leader_addr.parse().unwrap());

            let id = mock_identity();
            // Create follower who knows about leader Node 2
            let initial_state = RaftNodeState::Follower(RaftNode::<Follower>::new(id));
            let follower = initial_state.into_follower(0, Some(NodeId::new(2)));

            let dispatcher = mock_dispatcher(follower, mock_peer_manager(&peers));
            let req = Request::new(ProposeMutationRequest {
                cluster_id: "test-cluster".to_string(),
                ..Default::default()
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.leader_hint.contains(leader_addr));
            assert!(response.error_message.contains("NodeID 2"));
        }

        #[tokio::test]
        async fn returns_rejected_when_candidate() {
            let id = mock_identity();
            let follower = RaftNode::<Follower>::new(id);
            let candidate = RaftNodeState::Candidate(follower.into_candidate());

            let dispatcher = mock_dispatcher(candidate, mock_peer_manager(&HashMap::new()));
            let req = Request::new(ProposeMutationRequest {
                cluster_id: "test-cluster".to_string(),
                ..Default::default()
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.error_message.contains("Election in progress"));
        }
    }
}
