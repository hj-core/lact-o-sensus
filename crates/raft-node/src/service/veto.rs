use std::fmt::Debug;
use std::time::Duration;

use async_trait::async_trait;
use common::proto::v1::app::EvaluateProposalRequest;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::policy_service_client::PolicyServiceClient;
use thiserror::Error;
use tonic::Request;
use tonic::transport::Channel;

#[derive(Debug, Error)]
pub enum VetoError {
    #[error("AI evaluation timed out after {0:?}")]
    Timeout(Duration),

    #[error("AI evaluation RPC failed: {0}")]
    RpcFailure(String),
}

/// Result of an AI Veto evaluation.
#[derive(Debug, Clone)]
pub struct VetoOutcome {
    pub is_approved: bool,
    pub category_assignment: String,
    pub moral_justification: String,
}

/// Internal bridge for communicating with the AI Veto Node.
///
/// This trait decouples the Raft Leader from the specific gRPC
/// implementation of the Policy Service.
#[async_trait]
pub trait VetoRelay: Debug + Send + Sync {
    /// Evaluates a proposed mutation against the current inventory and "moral"
    /// heuristics.
    async fn evaluate(
        &self,
        client_id: String,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
        timeout: Duration,
    ) -> Result<VetoOutcome, VetoError>;
}

/// A gRPC-backed implementation of the VetoRelay.
#[derive(Debug, Clone)]
pub struct GrpcVetoRelay {
    client: PolicyServiceClient<Channel>,
}

impl GrpcVetoRelay {
    pub fn new(channel: Channel) -> Self {
        Self {
            client: PolicyServiceClient::new(channel),
        }
    }
}

#[async_trait]
impl VetoRelay for GrpcVetoRelay {
    async fn evaluate(
        &self,
        client_id: String,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
        timeout: Duration,
    ) -> Result<VetoOutcome, VetoError> {
        let mut client = self.client.clone();

        let mut request = Request::new(EvaluateProposalRequest {
            client_id,
            intent: Some(intent.clone()),
            current_inventory: current_inventory.to_vec(),
            request_context: "AI Policy Evaluation".to_string(),
        });
        request.set_timeout(timeout);

        let response = client
            .evaluate_proposal(request)
            .await
            .map_err(|e| {
                if e.code() == tonic::Code::DeadlineExceeded {
                    VetoError::Timeout(timeout)
                } else {
                    VetoError::RpcFailure(e.to_string())
                }
            })?
            .into_inner();

        Ok(VetoOutcome {
            is_approved: response.is_approved,
            category_assignment: response.category_assignment,
            moral_justification: response.moral_justification,
        })
    }
}
