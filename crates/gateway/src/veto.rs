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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
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
    // --- Semantic metadata resolved by the Oracle ---
    pub resolved_item_key: String,
    pub suggested_display_name: String,
    pub resolved_unit: String,
    /// Stabilization data (Decimal string)
    pub conversion_multiplier_to_base: String,
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
        max_justification_len: usize,
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

    /// Truncates the moral justification to a safe limit while preserving
    /// valid character boundaries.
    fn trim_justification(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_string()
        } else {
            // Find the last valid character boundary within our limit
            let mut end = max_len;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &s[..end])
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
        max_justification_len: usize,
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
            moral_justification: Self::trim_justification(
                &response.moral_justification,
                max_justification_len,
            ),
            resolved_item_key: response.resolved_item_key,
            suggested_display_name: response.suggested_display_name,
            resolved_unit: response.resolved_unit,
            conversion_multiplier_to_base: response.conversion_multiplier_to_base,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod trim_justification {
        use super::*;

        #[test]
        fn preserves_short_strings() {
            let short = "Clinical approval granted.";
            assert_eq!(GrpcVetoRelay::trim_justification(short, 100), short);
        }

        #[test]
        fn truncates_long_strings_to_limit() {
            let long = "a".repeat(100);
            let trimmed = GrpcVetoRelay::trim_justification(&long, 10);
            assert!(trimmed.len() <= 13); // 10 + 3 for ellipsis
            assert!(trimmed.ends_with("..."));
            assert_eq!(trimmed, "aaaaaaaaaa...");
        }

        #[test]
        fn respects_unicode_boundaries() {
            // "🦀" is 4 bytes.
            let s = "🦀🦀🦀";
            // 6 bytes would be in the middle of the second crab
            let trimmed = GrpcVetoRelay::trim_justification(s, 6);
            assert_eq!(trimmed, "🦀..."); // Truncated after first crab (4 bytes)
        }
    }
}
