use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::CommittedMutation;
use common::proto::v1::MutationIntent;
use common::proto::v1::MutationStatus;
use common::proto::v1::OperationType;
use common::proto::v1::ProposeMutationRequest;
use common::proto::v1::ProposeMutationResponse;
use common::proto::v1::QueryStateRequest;
use common::proto::v1::QueryStateResponse;
use common::proto::v1::QueryStatus;
use common::proto::v1::ingress_service_server::IngressService;
use common::types::ClientId;
use common::types::SequenceId;
use prost::Message;
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
use crate::node::RaftError;
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

    /// Helper to construct a standard "Rejected/Redirection" response when the
    /// node is a Follower for mutations.
    fn mutation_follower_response(
        &self,
        node: &RaftNode<Follower>,
    ) -> Response<ProposeMutationResponse> {
        let (leader_hint, error_message) = self.redirection_hint(node);
        Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint,
            error_message,
        })
    }

    /// Helper to construct a standard "Rejected/Redirection" response when the
    /// node is a Follower for QueryState.
    fn query_follower_response(&self, node: &RaftNode<Follower>) -> Response<QueryStateResponse> {
        let (leader_hint, error_message) = self.redirection_hint(node);
        Response::new(QueryStateResponse {
            items: Vec::new(),
            current_state_version: 0,
            status: QueryStatus::Rejected as i32,
            leader_hint,
            error_message,
        })
    }

    /// Normalizes and validates the user's intent before it is processed by
    /// the AI or added to the consensus log.
    fn normalize_intent(&self, intent: &mut MutationIntent) -> Result<(), Status> {
        intent.item_key = intent.item_key.trim().to_lowercase();
        intent.quantity = intent.quantity.trim().to_string();

        if let Some(unit) = intent.unit.as_mut() {
            *unit = unit.trim().to_lowercase();
        }

        if intent.item_key.is_empty() {
            return Err(Status::invalid_argument("item_key cannot be empty"));
        }

        if intent.quantity.is_empty() && intent.operation != OperationType::Delete as i32 {
            return Err(Status::invalid_argument("quantity cannot be empty"));
        }

        Ok(())
    }
}

impl ServiceState for IngressDispatcher {
    fn identity_arc(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }
}

#[tonic::async_trait]
impl IngressService for IngressDispatcher {
    async fn propose_mutation(
        &self,
        request: Request<ProposeMutationRequest>,
    ) -> Result<Response<ProposeMutationResponse>, Status> {
        let req = request.into_inner();

        let sequence_id = SequenceId::new(req.sequence_id);
        let client_id = req
            .client_id
            .parse::<ClientId>()
            .map_err(|e| Status::invalid_argument(format!("Invalid client_id: {}", e)))?;

        let span = info_span!("propose_mutation", client = %client_id, seq = %sequence_id);
        let _enter = span.enter();

        // --- Phase 1: Leadership & Normalization (Read Lock) ---
        let intent = {
            let state_guard = self.state.read().await;
            self.verify_node_integrity(&state_guard)?;

            match &*state_guard {
                RaftNodeState::Leader(_) => {
                    // TODO: Phase 6 - Exactly-Once Semantics (EOS)
                    // Check Session Table here before talking to AI.

                    let mut intent = req.intent.clone().ok_or_else(|| {
                        Status::invalid_argument("ProposeMutationRequest is missing 'intent' field")
                    })?;
                    self.normalize_intent(&mut intent)?;
                    intent
                }
                RaftNodeState::Candidate(_) => {
                    return Ok(Response::new(ProposeMutationResponse {
                        status: MutationStatus::Rejected as i32,
                        state_version: 0,
                        leader_hint: String::new(),
                        error_message: "Election in progress. No leader established.".to_string(),
                    }));
                }
                RaftNodeState::Follower(node) => return Ok(self.mutation_follower_response(node)),
                _ => return Err(self.poisoned_status()),
            }
        }; // READ LOCK DROPPED

        // --- Phase 2: Policy Egress (I/O - No Locks) ---
        info!("Triggering AI Veto evaluation for normalized intent...");
        let outcome = self
            .veto_relay
            .evaluate(
                req.client_id.clone(),
                &intent,
                &[], // Inventory store implemented in Phase 5
                self.veto_timeout,
            )
            .await;

        let veto = match outcome {
            Ok(v) => {
                if !v.is_approved {
                    info!("Mutation VETOED by AI: {}", v.moral_justification);
                    return Ok(Response::new(ProposeMutationResponse {
                        status: MutationStatus::Vetoed as i32,
                        state_version: 0,
                        leader_hint: String::new(),
                        error_message: v.moral_justification,
                    }));
                }
                v
            }
            Err(VetoError::Timeout(d)) => {
                warn!("AI Veto evaluation timed out after {:?}", d);
                return Ok(Response::new(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: "AI evaluation timed out. Please retry shortly.".to_string(),
                }));
            }
            Err(e) => {
                error!("AI Veto infrastructure failure: {}", e);
                return Err(Status::internal("Internal policy engine failure"));
            }
        };

        // --- Phase 3: Consensus Proposal (Write Lock) ---
        // Resolve the intent into a finalized ledger entry.
        // TODO: Phase 5 - Item Store Integration (Delta Calculation)
        let mutation = CommittedMutation::new(
            &client_id,
            sequence_id,
            intent.item_key.clone(),
            intent.quantity.clone(),
            veto.category_assignment,
            intent.unit.unwrap_or_default(),
            intent.operation == OperationType::Delete as i32,
            std::time::SystemTime::now(),
        );

        let mut command = Vec::new();
        mutation
            .encode(&mut command)
            .map_err(|e| Status::internal(e.to_string()))?;

        // Precision Synchronization: Get the signal handle BEFORE insertion
        let commit_signal = self
            .state
            .read()
            .await
            .commit_signal()
            .map_err(|_| self.poisoned_status())?;

        let proposal_index = {
            let mut guard = self.state.write().await;
            match guard.propose(command) {
                Ok(idx) => idx,
                Err(RaftError::NotLeader) => {
                    // Node was demoted while we were talking to the AI!
                    if let RaftNodeState::Follower(node) = &*guard {
                        return Ok(self.mutation_follower_response(node));
                    }
                    return Err(Status::unavailable("Leadership lost during evaluation"));
                }
                _ => return Err(self.poisoned_status()),
            }
        }; // WRITE LOCK DROPPED

        // --- Phase 4: Wait for Quorum (Reactive Sync) ---
        info!(
            "Mutation index {} appended. Waiting for quorum...",
            proposal_index
        );
        loop {
            // Register interest before checking state to avoid "Lost Wakeup"
            let next_notification = commit_signal.notified();

            {
                let guard = self.state.read().await;
                match &*guard {
                    RaftNodeState::Leader(node) => {
                        if node.commit_index() >= proposal_index {
                            break;
                        }
                    }
                    RaftNodeState::Follower(node) => {
                        info!("Demoted while waiting for quorum. Redirecting...");
                        return Ok(self.mutation_follower_response(node));
                    }
                    _ => return Err(self.poisoned_status()),
                }
            }

            next_notification.await;
        }

        info!("Mutation index {} committed successfully.", proposal_index);
        Ok(Response::new(ProposeMutationResponse {
            status: MutationStatus::Committed as i32,
            state_version: proposal_index.value(),
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }

    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let _req = request.into_inner();

        let state_guard = self.state.read().await;
        self.verify_node_integrity(&state_guard)?;

        let span = info_span!("query_state");
        let _enter = span.enter();

        match &*state_guard {
            RaftNodeState::Follower(node) => {
                info!("Redirecting query: Node is a Follower.");
                Ok(self.query_follower_response(node))
            }
            RaftNodeState::Candidate(_) => Ok(Response::new(QueryStateResponse {
                items: Vec::new(),
                current_state_version: 0,
                status: QueryStatus::Rejected as i32,
                leader_hint: String::new(),
                error_message: "Election in progress. No leader established.".to_string(),
            })),
            RaftNodeState::Leader(_) => {
                // TODO: Phase 5 - Implement State Machine queries (Item Store)
                Ok(Response::new(QueryStateResponse {
                    items: Vec::new(),
                    current_state_version: 0,
                    status: QueryStatus::Success as i32,
                    leader_hint: String::new(),
                    error_message: String::new(),
                }))
            }
            _ => Err(self.poisoned_status()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::Term;

    use super::*;
    use crate::node::Follower;
    use crate::node::RaftNode;
    use crate::service::veto::VetoError;
    use crate::service::veto::VetoOutcome;
    use crate::service::veto::VetoRelay;

    #[derive(Debug)]
    struct TestVetoRelay;

    #[tonic::async_trait]
    impl VetoRelay for TestVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &common::proto::v1::MutationIntent,
            _current_inventory: &[common::proto::v1::GroceryItem],
            _timeout: Duration,
        ) -> Result<VetoOutcome, VetoError> {
            Ok(VetoOutcome {
                is_approved: true,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Test approval".to_string(),
            })
        }
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn mock_peer_manager(
        identity: Arc<NodeIdentity>,
        peers: &HashMap<NodeId, String>,
    ) -> Arc<PeerManager> {
        Arc::new(PeerManager::new(identity, peers).unwrap())
    }

    fn mock_dispatcher(state: RaftNodeState, peer_manager: Arc<PeerManager>) -> IngressDispatcher {
        let identity = state.identity_arc().unwrap().clone();
        IngressDispatcher::new(
            identity,
            Arc::new(RwLock::new(state)),
            peer_manager,
            Arc::new(TestVetoRelay),
            Duration::from_secs(1),
        )
    }

    mod propose_mutation {
        use super::*;

        #[tokio::test]
        async fn returns_rejected_when_follower_leader_unknown() {
            let id = mock_identity();
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
            let dispatcher = mock_dispatcher(node, mock_peer_manager(id, &HashMap::new()));
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: None,
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.error_message.contains("leader is unknown"));
            assert!(response.leader_hint.is_empty());
        }

        #[tokio::test]
        async fn returns_hint_when_follower_leader_known() {
            let mut peers = HashMap::new();
            let leader_addr = "http://127.0.0.1:50052";
            peers.insert(NodeId::new(2), leader_addr.to_string());

            let id = mock_identity();
            // Create follower who knows about leader Node 2
            let initial_state = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
            let follower = initial_state.into_follower(Term::ZERO, Some(NodeId::new(2)));

            let dispatcher = mock_dispatcher(follower, mock_peer_manager(id, &peers));
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: None,
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.leader_hint.contains(leader_addr));
            assert!(response.error_message.contains("NodeID 2"));
        }

        #[tokio::test]
        async fn returns_rejected_when_candidate() {
            let id = mock_identity();
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = RaftNodeState::Candidate(follower.into_candidate());

            let dispatcher = mock_dispatcher(candidate, mock_peer_manager(id, &HashMap::new()));
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: None,
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(response.error_message.contains("established"));
        }
    }

    mod query_state {
        use super::*;

        #[tokio::test]
        async fn returns_rejected_when_follower_leader_unknown() {
            let id = mock_identity();
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
            let dispatcher = mock_dispatcher(node, mock_peer_manager(id, &HashMap::new()));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Rejected as i32);
            assert!(response.leader_hint.is_empty());
        }

        #[tokio::test]
        async fn returns_hint_when_follower_leader_known() {
            let mut peers = HashMap::new();
            let leader_addr = "http://127.0.0.1:50052";
            peers.insert(NodeId::new(2), leader_addr.to_string());

            let id = mock_identity();
            let initial_state = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
            let follower = initial_state.into_follower(Term::ZERO, Some(NodeId::new(2)));

            let dispatcher = mock_dispatcher(follower, mock_peer_manager(id, &peers));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Rejected as i32);
            assert!(response.leader_hint.contains(leader_addr));
        }

        #[tokio::test]
        async fn returns_rejected_when_candidate() {
            let id = mock_identity();
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = RaftNodeState::Candidate(follower.into_candidate());

            let dispatcher = mock_dispatcher(candidate, mock_peer_manager(id, &HashMap::new()));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Rejected as i32);
        }

        #[tokio::test]
        async fn returns_success_when_leader() {
            let id = mock_identity();
            let follower = RaftNode::<Follower>::new(id.clone());
            let leader = RaftNodeState::Leader(follower.into_candidate().into_leader(Vec::new()));

            let dispatcher = mock_dispatcher(leader, mock_peer_manager(id, &HashMap::new()));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Success as i32);
        }
    }
}
