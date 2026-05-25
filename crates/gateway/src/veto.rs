//! # Gateway Veto: The Semantic Oracle Relay
//!
//! This module implements the client-side relay for the AI Veto Oracle.
//! It corresponds to **Layer 3: Semantic Oracle** of the Defensive Mutation
//! Lifecycle (ADR 007).
//!
//! ## Core Responsibilities
//!
//! 1. **Semantic Resolution:** Bridges the leader node to the external AI
//!    service for item key mapping and moral evaluation.
//! 2. **Causal Integrity (ADR 010):** Enforces Byzantine verification of
//!    distributed TraceIds to prevent trace grafting.
//! 3. **Audit Trimming:** Caps the length of AI-provided justifications to
//!    prevent consensus log bloat while maintaining character boundary safety.

use std::fmt::Debug;
use std::time::Duration;

use async_trait::async_trait;
use common::proto::v1::app::EvaluateProposalRequest;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::policy_service_client::PolicyServiceClient;
use common::rpc::TraceInterceptor;
use common::types::ClientId;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use thiserror::Error;
use tonic::Request;
use tonic::transport::Channel;
use tracing::Instrument;
use tracing::info_span;
use tracing::warn;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VetoError {
    #[error("AI evaluation timed out after {0:?}")]
    Timeout(Duration),

    #[error("AI evaluation RPC failed: {0}")]
    RpcFailure(String),

    #[error("Causal Integrity Violation: TraceId mismatch or missing")]
    CausalIntegrityViolation,
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
        client_id: ClientId,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
        timeout: Duration,
        max_justification_len: usize,
        trace_id: TraceId,
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
        client_id: ClientId,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
        timeout: Duration,
        max_justification_len: usize,
        trace_id: TraceId,
    ) -> Result<VetoOutcome, VetoError> {
        let mut client = self.client.clone();

        let mut request = Request::new(EvaluateProposalRequest::new(
            &client_id,
            intent.clone(),
            current_inventory.to_vec(),
            "AI Policy Evaluation".to_string(),
        ));
        request.set_timeout(timeout);

        // Explicit Outbound Propagation (ADR 010)
        TraceInterceptor::inject_trace_id_into_request(&mut request, trace_id)
            .map_err(|e| VetoError::RpcFailure(format!("Telemetry injection failed: {}", e)))?;

        let span = info_span!(
            target: ClinicalTarget::ClinicalOracle.as_str(),
            "oracle_rpc_call",
            %trace_id,
            client_id = %client_id.truncated(),
            timeout = ?timeout
        );

        async {
            let response = client.evaluate_proposal(request).await.map_err(|e| {
                if e.code() == tonic::Code::DeadlineExceeded {
                    VetoError::Timeout(timeout)
                } else {
                    VetoError::RpcFailure(e.to_string())
                }
            })?;

            // Byzantine Resilience: Verify returned trace_id matches the one we sent.
            // If the AI Veto Node (untrusted) returns a different ID or strips it, reject.
            match TraceInterceptor::extract_trace_id_from_response(&response) {
                Some(returned_id) if returned_id == trace_id => {}
                _ => {
                    warn!(
                        target: ClinicalTarget::ClinicalTelemetry.as_str(),
                        expected = %trace_id,
                        "Causal Integrity Violation: AI Veto Node returned mismatched or missing TraceId"
                    );
                    return Err(VetoError::CausalIntegrityViolation);
                }
            }

            let response = response.into_inner();

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
        .instrument(span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;

    use common::proto::v1::app::EvaluateProposalResponse;
    use common::proto::v1::app::policy_service_server::PolicyService;
    use common::proto::v1::app::policy_service_server::PolicyServiceServer;
    use common::rpc::TraceInterceptor;
    use tokio::sync::oneshot;
    use tonic::Response;
    use tonic::Status;
    use tonic::transport::Server;

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

    mod grpc_veto_relay {
        use super::*;

        struct MockPolicyService {
            response: Arc<Mutex<Result<EvaluateProposalResponse, Status>>>,
            trace_id_to_return: Arc<Mutex<Option<TraceId>>>,
        }

        #[async_trait]
        impl PolicyService for MockPolicyService {
            async fn evaluate_proposal(
                &self,
                _request: Request<EvaluateProposalRequest>,
            ) -> Result<Response<EvaluateProposalResponse>, Status> {
                let res_result = self.response.lock().unwrap().clone();
                let mut res = res_result.map(Response::new)?;

                // If we have a trace_id to return, inject it.
                // Otherwise, the response won't have one (simulating a strip/miss).
                if let Some(tid) = *self.trace_id_to_return.lock().unwrap() {
                    TraceInterceptor::inject_trace_id_into_response(&mut res, tid)
                        .map_err(|e| Status::internal(e.to_string()))?;
                }

                Ok(res)
            }
        }

        async fn setup_test_relay(
            port: u16,
        ) -> (GrpcVetoRelay, MockPolicyService, oneshot::Sender<()>) {
            let (tx, rx) = oneshot::channel::<()>();
            let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

            let mock_service = MockPolicyService {
                response: Arc::new(Mutex::new(Ok(EvaluateProposalResponse::default()))),
                trace_id_to_return: Arc::new(Mutex::new(None)),
            };

            let service = PolicyServiceServer::new(MockPolicyService {
                response: mock_service.response.clone(),
                trace_id_to_return: mock_service.trace_id_to_return.clone(),
            });

            tokio::spawn(async move {
                Server::builder()
                    .add_service(service)
                    .serve_with_shutdown(addr, async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
            });

            // Small delay to ensure server started
            tokio::time::sleep(Duration::from_millis(50)).await;

            let channel = Channel::from_shared(format!("http://{}", addr))
                .unwrap()
                .connect()
                .await
                .unwrap();

            (GrpcVetoRelay::new(channel), mock_service, tx)
        }

        mod evaluate {
            use super::*;

            #[tokio::test]
            async fn should_return_outcome_when_rpc_succeeds_with_correct_trace() {
                let (relay, mock, _shutdown) = setup_test_relay(50061).await;
                let trace_id = TraceId::generate();
                let client_id = ClientId::generate();

                *mock.response.lock().unwrap() = Ok(EvaluateProposalResponse {
                    is_approved: true,
                    category_assignment: "Primary Flora".to_string(),
                    moral_justification: "Approved".to_string(),
                    resolved_item_key: "apple".to_string(),
                    suggested_display_name: "Apple".to_string(),
                    resolved_unit: "units".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                });
                *mock.trace_id_to_return.lock().unwrap() = Some(trace_id);

                let result = relay
                    .evaluate(
                        client_id,
                        &MutationIntent::default(),
                        &[],
                        Duration::from_secs(1),
                        100,
                        trace_id,
                    )
                    .await
                    .unwrap();

                assert!(result.is_approved);
                assert_eq!(result.resolved_item_key, "apple");
            }

            #[tokio::test]
            async fn should_return_causal_integrity_violation_when_trace_id_mismatches() {
                let (relay, mock, _shutdown) = setup_test_relay(50062).await;
                let trace_id = TraceId::generate();
                let wrong_trace_id = TraceId::generate();

                *mock.response.lock().unwrap() = Ok(EvaluateProposalResponse::default());
                *mock.trace_id_to_return.lock().unwrap() = Some(wrong_trace_id);

                let result = relay
                    .evaluate(
                        ClientId::generate(),
                        &MutationIntent::default(),
                        &[],
                        Duration::from_secs(1),
                        100,
                        trace_id,
                    )
                    .await;

                assert_eq!(result.unwrap_err(), VetoError::CausalIntegrityViolation);
            }

            #[tokio::test]
            async fn should_return_causal_integrity_violation_when_trace_id_is_missing() {
                let (relay, mock, _shutdown) = setup_test_relay(50063).await;
                let trace_id = TraceId::generate();

                *mock.response.lock().unwrap() = Ok(EvaluateProposalResponse::default());
                *mock.trace_id_to_return.lock().unwrap() = None; // Missing TraceId

                let result = relay
                    .evaluate(
                        ClientId::generate(),
                        &MutationIntent::default(),
                        &[],
                        Duration::from_secs(1),
                        100,
                        trace_id,
                    )
                    .await;

                assert_eq!(result.unwrap_err(), VetoError::CausalIntegrityViolation);
            }

            #[tokio::test]
            async fn should_return_timeout_error_when_deadline_exceeded() {
                let (relay, mock, _shutdown) = setup_test_relay(50064).await;
                let trace_id = TraceId::generate();
                let timeout = Duration::from_millis(10);

                // Use the mock service to return an explicit DeadlineExceeded status
                *mock.response.lock().unwrap() = Err(Status::deadline_exceeded("Timed out"));

                let result = relay
                    .evaluate(
                        ClientId::generate(),
                        &MutationIntent::default(),
                        &[],
                        timeout,
                        100,
                        trace_id,
                    )
                    .await;

                assert_eq!(result.unwrap_err(), VetoError::Timeout(timeout));
            }

            #[tokio::test]
            async fn should_return_rpc_failure_when_server_returns_internal_error() {
                let (relay, mock, _shutdown) = setup_test_relay(50065).await;

                *mock.response.lock().unwrap() = Err(Status::internal("Server crash"));

                let result = relay
                    .evaluate(
                        ClientId::generate(),
                        &MutationIntent::default(),
                        &[],
                        Duration::from_secs(1),
                        100,
                        TraceId::generate(),
                    )
                    .await;

                match result.unwrap_err() {
                    VetoError::RpcFailure(msg) => assert!(msg.contains("Server crash")),
                    _ => panic!("Expected RpcFailure"),
                }
            }
        }
    }
}
