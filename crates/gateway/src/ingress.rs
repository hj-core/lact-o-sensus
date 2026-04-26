use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::OperationType;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_server::IngressService;
use common::raft_api::ConsensusStatus;
use common::raft_api::RaftHandle;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use prost::Message;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;

use crate::veto::VetoError;
use crate::veto::VetoOutcome;
use crate::veto::VetoRelay;

/// Implementation of the external client ingress RPCs.
///
/// This service handles user mutations and state queries, enforcing
/// cluster identity and redirecting clients to the current leader.
#[derive(Debug)]
pub struct IngressDispatcher {
    raft_handle: Arc<dyn RaftHandle>,
    veto_relay: Arc<dyn VetoRelay>,
    veto_timeout: Duration,
}

#[tonic::async_trait]
impl IngressService for IngressDispatcher {
    /// High-level orchestrator for user mutations.
    /// Implements the Defensive Onion pipeline (ADR 007).
    async fn propose_mutation(
        &self,
        request: Request<ProposeMutationRequest>,
    ) -> Result<Response<ProposeMutationResponse>, Status> {
        let req = request.into_inner();

        let sequence_id = SequenceId::new(req.sequence_id);
        let client_id = req
            .client_id
            .parse::<ClientId>()
            .map_err(|e| self.invalid_argument(format!("Invalid client_id: {}", e)))?;

        let span = info_span!("propose_mutation", client = %client_id, seq = %sequence_id);
        let _enter = span.enter();

        // --- Phase 0: Leadership Authority & Consensus Status ---
        let status = self.raft_handle.consensus_status().await;
        if !status.is_leader {
            return Ok(self.rejection_response_with_status(status));
        }

        // --- Phase 1: Syntactic Normalization ---
        let mut intent = req.intent.clone().ok_or_else(|| {
            self.invalid_argument("ProposeMutationRequest is missing 'intent' field")
        })?;
        self.normalize_intent(&mut intent)?;

        // --- Phase 2: Semantic AI Policy Egress ---
        let veto = self.evaluate_policy(req.client_id, &intent).await?;
        if !veto.is_approved {
            return Ok(Response::new(ProposeMutationResponse {
                status: MutationStatus::Vetoed as i32,
                state_version: 0,
                leader_hint: String::new(),
                error_message: veto.moral_justification,
            }));
        }

        // --- Phase 3 & 4: Consensus Proposal & Quorum ---
        let proposal_index = self
            .commit_to_consensus(&client_id, sequence_id, intent, veto)
            .await?;

        info!("Mutation index {} committed successfully.", proposal_index);
        Ok(Response::new(ProposeMutationResponse {
            status: MutationStatus::Committed as i32,
            state_version: proposal_index.value(),
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }

    /// High-level orchestrator for state queries.
    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let _req = request.into_inner();

        let span = info_span!("query_state");
        let _enter = span.enter();

        let status = self.raft_handle.consensus_status().await;
        if !status.is_leader {
            return Ok(Response::new(QueryStateResponse {
                items: Vec::new(),
                current_state_version: 0,
                status: QueryStatus::Rejected as i32,
                leader_hint: status.leader_hint,
                error_message: status.rejection_reason,
            }));
        }

        // TODO: Phase 5 - Implement State Machine queries (Item Store)
        Ok(Response::new(QueryStateResponse {
            items: Vec::new(),
            current_state_version: 0,
            status: QueryStatus::Success as i32,
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }
}

impl IngressDispatcher {
    pub fn new(
        raft_handle: Arc<dyn RaftHandle>,
        veto_relay: Arc<dyn VetoRelay>,
        veto_timeout: Duration,
    ) -> Self {
        Self {
            raft_handle,
            veto_relay,
            veto_timeout,
        }
    }

    /// Helper to construct a standard "Rejected/Redirection" response from a
    /// status snapshot.
    fn rejection_response_with_status(
        &self,
        status: ConsensusStatus,
    ) -> Response<ProposeMutationResponse> {
        Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint: status.leader_hint,
            error_message: status.rejection_reason,
        })
    }

    /// Returns a standard gRPC InvalidArgument status.
    fn invalid_argument(&self, msg: impl Into<String>) -> Status {
        Status::invalid_argument(msg)
    }

    /// Returns a standard gRPC Internal status.
    fn internal_error(&self, msg: impl Into<String>) -> Status {
        Status::internal(msg)
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
            return Err(self.invalid_argument("item_key cannot be empty"));
        }

        if intent.quantity.is_empty() && intent.operation != OperationType::Delete as i32 {
            return Err(self.invalid_argument("quantity cannot be empty"));
        }

        Ok(())
    }

    /// Phase 2 Implementation: Semantic validation via AI Veto Relay.
    async fn evaluate_policy(
        &self,
        client_id: String,
        intent: &MutationIntent,
    ) -> Result<VetoOutcome, Status> {
        info!("Triggering AI Veto evaluation for normalized intent...");
        let outcome = self
            .veto_relay
            .evaluate(
                client_id,
                intent,
                &[], // Inventory store implemented in Phase 5
                self.veto_timeout,
            )
            .await;

        match outcome {
            Ok(v) => {
                if !v.is_approved {
                    info!("Mutation VETOED by AI: {}", v.moral_justification);
                }
                Ok(v)
            }
            Err(VetoError::Timeout(d)) => {
                warn!("AI Veto evaluation timed out after {:?}", d);
                Err(Status::deadline_exceeded(
                    "AI evaluation timed out. Please retry shortly.",
                ))
            }
            Err(e) => {
                error!("AI Veto infrastructure failure: {}", e);
                Err(self.internal_error("Internal policy engine failure"))
            }
        }
    }

    /// Phase 3 & 4 Implementation: Consensus and Quorum.
    async fn commit_to_consensus(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
        intent: MutationIntent,
        veto: VetoOutcome,
    ) -> Result<LogIndex, Status> {
        let mutation = CommittedMutation::new(
            client_id,
            sequence_id,
            intent.item_key.clone(),       // resolved_item_key (stub)
            "PENDING_PHASE_8".to_string(), // suggested_display_name
            intent.quantity.clone(),       // updated_base_quantity (stub)
            intent.unit.clone().unwrap_or_default(), // base_unit (stub)
            intent.unit.clone().unwrap_or_default(), // display_unit (stub)
            intent.category.clone().unwrap_or_default(), // updated_category (stub)
            format!("intent: {:?}", intent), // raw_user_input
            "Automatic approval (Step 7 stub)".to_string(), // moral_justification
            intent.operation == OperationType::Delete as i32,
            std::time::SystemTime::now(),
        );

        let mut command = Vec::new();
        mutation
            .encode(&mut command)
            .map_err(|e| self.internal_error(e.to_string()))?;

        let proposal_index = self.raft_handle.propose(command).await?;

        info!(
            "Mutation index {} appended. Waiting for quorum...",
            proposal_index
        );

        self.raft_handle.await_commit(proposal_index).await?;
        Ok(proposal_index)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    #[derive(Debug, Default)]
    struct MockRaftHandle {
        is_leader: bool,
        leader_hint: String,
        rejection_reason: String,
        proposals: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl RaftHandle for MockRaftHandle {
        async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, Status> {
            self.proposals.lock().unwrap().push(data);
            Ok(LogIndex::new(1))
        }

        async fn await_commit(&self, _index: LogIndex) -> Result<(), Status> {
            Ok(())
        }

        async fn consensus_status(&self) -> ConsensusStatus {
            ConsensusStatus {
                is_leader: self.is_leader,
                leader_hint: self.leader_hint.clone(),
                rejection_reason: self.rejection_reason.clone(),
            }
        }
    }

    #[derive(Debug, Default)]
    struct MockVetoRelay {
        approved: bool,
        error: Option<VetoError>,
    }

    #[async_trait]
    impl VetoRelay for MockVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &MutationIntent,
            _current_inventory: &[common::proto::v1::app::GroceryItem],
            _timeout: Duration,
        ) -> Result<VetoOutcome, VetoError> {
            if let Some(err) = &self.error {
                return Err(err.clone());
            }
            Ok(VetoOutcome {
                is_approved: self.approved,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Mock justification".to_string(),
            })
        }
    }

    fn mock_dispatcher(
        raft_handle: Arc<dyn RaftHandle>,
        veto_relay: Arc<dyn VetoRelay>,
    ) -> IngressDispatcher {
        IngressDispatcher::new(raft_handle, veto_relay, Duration::from_secs(1))
    }

    mod propose_mutation {
        use super::*;

        fn successful_raft() -> Arc<MockRaftHandle> {
            Arc::new(MockRaftHandle {
                is_leader: true,
                ..Default::default()
            })
        }

        fn successful_veto() -> Arc<MockVetoRelay> {
            Arc::new(MockVetoRelay {
                approved: true,
                ..Default::default()
            })
        }

        // --- Phase 0: Leadership ---

        #[tokio::test]
        async fn returns_rejected_when_not_leader() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: false,
                leader_hint: "http://leader:50051".to_string(),
                rejection_reason: "Node is a Follower".to_string(),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: None,
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert_eq!(response.leader_hint, "http://leader:50051");
            assert!(response.error_message.contains("Follower"));
        }

        // --- Phase 1: Syntactic ---

        #[tokio::test]
        async fn normalizes_intent_before_proposal() {
            let raft = successful_raft();
            let dispatcher = mock_dispatcher(raft.clone(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "  BANANAS  ".to_string(),
                    quantity: " 5 ".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let _ = dispatcher.propose_mutation(req).await.unwrap();

            let proposals = raft.proposals.lock().unwrap();
            assert_eq!(proposals.len(), 1);
            let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
            assert_eq!(mutation.resolved_item_key, "bananas");
            assert_eq!(mutation.updated_base_quantity, "5");
        }

        #[tokio::test]
        async fn rejects_empty_item_key() {
            let dispatcher = mock_dispatcher(successful_raft(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "   ".to_string(),
                    quantity: "5".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }

        // --- Phase 2: Semantic & Infrastructure ---

        #[tokio::test]
        async fn returns_vetoed_when_ai_rejects() {
            let veto = Arc::new(MockVetoRelay {
                approved: false,
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(successful_raft(), veto);
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: "5".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(response.error_message.contains("Mock justification"));
        }

        #[tokio::test]
        async fn returns_error_on_ai_timeout() {
            let veto = Arc::new(MockVetoRelay {
                error: Some(VetoError::Timeout(Duration::from_secs(1))),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(successful_raft(), veto);
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: "5".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        }

        // --- Phase 3: Consensus ---

        #[tokio::test]
        async fn returns_error_on_consensus_failure() {
            #[derive(Debug, Default)]
            struct FailingRaft;
            #[async_trait]
            impl RaftHandle for FailingRaft {
                async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, Status> {
                    Err(Status::internal("Consensus failure"))
                }

                async fn await_commit(&self, _index: LogIndex) -> Result<(), Status> {
                    Ok(())
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    ConsensusStatus {
                        is_leader: true,
                        ..Default::default()
                    }
                }
            }

            let dispatcher = mock_dispatcher(Arc::new(FailingRaft), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: "5".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }
    }

    mod query_state {
        use super::*;

        #[tokio::test]
        async fn returns_rejected_when_not_leader() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: false,
                leader_hint: "http://leader:50051".to_string(),
                rejection_reason: "Node is a Follower".to_string(),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, Arc::new(MockVetoRelay::default()));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Rejected as i32);
            assert_eq!(response.leader_hint, "http://leader:50051");
        }
    }
}
