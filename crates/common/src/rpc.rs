use std::sync::Arc;

use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::service::Interceptor;
use tracing::warn;

use crate::types::NodeId;
use crate::types::NodeIdentity;
use crate::types::trace::ClinicalTarget;
use crate::types::trace::TraceId;

/// Metadata header key for the cluster identifier.
pub const HEADER_CLUSTER_ID: &str = "x-cluster-id";
/// Metadata header key for the target node identifier.
pub const HEADER_TARGET_NODE_ID: &str = "x-target-node-id";
/// Metadata header key for the distributed trace identifier (ADR 010).
pub const HEADER_TRACE_ID: &str = "x-trace-id";

/// Centralized interceptor for verifying cluster and node identity (ADR 004).
///
/// This interceptor ensures that every incoming RPC contains the correct
/// `x-cluster-id` and `x-target-node-id` headers, preventing logical
/// misrouting and cross-cluster traffic leakage.
#[derive(Debug, Clone)]
pub struct IdentityInterceptor {
    identity: Arc<NodeIdentity>,
}

impl IdentityInterceptor {
    pub fn new(identity: Arc<NodeIdentity>) -> Self {
        Self { identity }
    }

    /// Injects identity headers into an outbound request for cluster isolation.
    pub fn inject_request<T>(
        request: &mut Request<T>,
        cluster_id: &crate::types::ClusterId,
        target_node_id: crate::types::NodeId,
    ) -> Result<(), Status> {
        let cluster_val = cluster_id
            .as_str()
            .parse()
            .map_err(|_| Status::internal("Failed to parse cluster_id for outbound header"))?;
        let node_val = target_node_id
            .to_string()
            .parse()
            .map_err(|_| Status::internal("Failed to parse target_node_id for outbound header"))?;

        request
            .metadata_mut()
            .insert(HEADER_CLUSTER_ID, cluster_val);
        request
            .metadata_mut()
            .insert(HEADER_TARGET_NODE_ID, node_val);
        Ok(())
    }
}

impl Interceptor for IdentityInterceptor {
    /// High-level orchestrator for RPC identity verification.
    /// Delegates to specialized sub-functions to maintain Information
    /// Hierarchy.
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.verify_cluster_id(&request)?;
        self.verify_target_node_id(&request)?;

        Ok(request)
    }
}

impl IdentityInterceptor {
    /// Mandatory Boundary Check: Ensures the request belongs to this logical
    /// cluster.
    fn verify_cluster_id(&self, request: &Request<()>) -> Result<(), Status> {
        let cluster_id_header = request
            .metadata()
            .get(HEADER_CLUSTER_ID)
            .ok_or_else(|| {
                Status::unauthenticated(format!("Missing mandatory {} header", HEADER_CLUSTER_ID))
            })?
            .to_str()
            .map_err(|_| Status::unauthenticated("Invalid cluster ID encoding"))?;

        if cluster_id_header != self.identity.cluster_id().as_str() {
            warn!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                expected = %self.identity.cluster_id(),
                got = %cluster_id_header,
                "ISOLATION MISMATCH: Cluster ID mismatch"
            );
            return Err(Status::unauthenticated("Cluster identity mismatch"));
        }

        Ok(())
    }

    /// Mandatory Routing Check: Ensures the request was intended for this
    /// specific node.
    fn verify_target_node_id(&self, request: &Request<()>) -> Result<(), Status> {
        let node_id_header = request
            .metadata()
            .get(HEADER_TARGET_NODE_ID)
            .ok_or_else(|| {
                Status::unauthenticated(format!(
                    "Missing mandatory {} header",
                    HEADER_TARGET_NODE_ID
                ))
            })?;

        let node_id_str = node_id_header
            .to_str()
            .map_err(|_| Status::unauthenticated("Invalid node ID encoding"))?;

        let target_node_id = node_id_str
            .parse::<NodeId>()
            .map_err(|_| Status::unauthenticated("Invalid node ID format"))?;

        if target_node_id != self.identity.node_id() {
            warn!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                expected = %self.identity.node_id(),
                got = %target_node_id,
                "ROUTING MISMATCH: Target node mismatch"
            );
            return Err(Status::unauthenticated("Target node identity mismatch"));
        }

        Ok(())
    }
}

/// Interceptor for distributed trace propagation (ADR 010).
///
/// Supports two modes:
/// 1. **Authoritative (Gateway):** Always generates a new `TraceId` (UUID v7),
///    effectively defining the "Clinical Birth" of a request within the cluster
///    fortress.
/// 2. **Propagative (Internal):** Extracts the `TraceId` from gRPC metadata
///    sent by trusted cluster peers.
#[derive(Debug, Clone)]
pub struct TraceInterceptor {
    authoritative: bool,
}

impl TraceInterceptor {
    /// Creates an authoritative interceptor that generates new Trace IDs.
    pub fn authoritative() -> Self {
        Self {
            authoritative: true,
        }
    }

    /// Creates a propagative interceptor that extracts existing Trace IDs.
    pub fn propagative() -> Self {
        Self {
            authoritative: false,
        }
    }

    /// Extracts a TraceId from a Request's extensions if present.
    fn extract_request<T>(request: &Request<T>) -> Option<TraceId> {
        request.extensions().get::<TraceId>().copied()
    }

    /// Mandates the presence of a TraceId in a Request's extensions.
    ///
    /// Returns the TraceId or a pre-configured gRPC Status if missing.
    /// Used by internal services to enforce clinical correlation (ADR 010).
    pub fn require_trace_id<T>(request: &Request<T>) -> Result<TraceId, Status> {
        Self::extract_request(request).ok_or_else(|| {
            Status::failed_precondition("Missing TraceId for distributed correlation")
        })
    }

    /// Extracts a TraceId from a Response's metadata (ADR 010).
    pub fn extract_response<T>(response: &Response<T>) -> Option<TraceId> {
        if let Some(trace_val) = response.metadata().get(HEADER_TRACE_ID)
            && let Ok(trace_str) = trace_val.to_str()
            && let Ok(trace_id) = trace_str.parse::<TraceId>()
        {
            return Some(trace_id);
        }
        None
    }

    /// Injects a TraceId into a Request's metadata for outbound propagation.
    pub fn inject_request<T>(request: &mut Request<T>, trace_id: TraceId) -> Result<(), Status> {
        let trace_val = trace_id
            .to_string()
            .parse()
            .map_err(|_| Status::internal("Failed to parse TraceId for outbound header"))?;
        request.metadata_mut().insert(HEADER_TRACE_ID, trace_val);
        Ok(())
    }

    /// Injects a TraceId into a Response's metadata for client feedback (ADR
    /// 010).
    pub fn inject_response<T>(response: &mut Response<T>, trace_id: TraceId) -> Result<(), Status> {
        let trace_val = trace_id
            .to_string()
            .parse()
            .map_err(|_| Status::internal("Failed to parse TraceId for outbound header"))?;
        response.metadata_mut().insert(HEADER_TRACE_ID, trace_val);
        Ok(())
    }
}

impl Default for TraceInterceptor {
    fn default() -> Self {
        Self::propagative()
    }
}

impl Interceptor for TraceInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if self.authoritative {
            // Gateway Authority: Always generate a new "Clinical Birth" ID.
            let trace_id = TraceId::generate();
            request.extensions_mut().insert(trace_id);
            Ok(request)
        } else {
            let trace_id = request
                .metadata()
                .get(HEADER_TRACE_ID)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<TraceId>().ok())
                .ok_or_else(|| {
                    warn!(
                        target: ClinicalTarget::ClinicalTelemetry.as_str(),
                        "Rejected request: Missing or malformed TraceId for internal propagation"
                    );
                    Status::failed_precondition(
                        "Missing or malformed TraceId for distributed correlation",
                    )
                })?;

            request.extensions_mut().insert(trace_id);
            Ok(request)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ClusterId;
    use crate::types::TraceId;

    fn mock_identity(cluster: &str, node_id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new(cluster).unwrap(),
            NodeId::new(node_id),
        ))
    }

    mod identity_interceptor {
        use super::*;

        fn authenticated_request(cluster: &str, node_id: u64) -> Request<()> {
            let mut req = Request::new(());
            req.metadata_mut()
                .insert(HEADER_CLUSTER_ID, cluster.parse().unwrap());
            req.metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, node_id.to_string().parse().unwrap());
            req
        }

        mod call {
            use super::*;

            #[test]
            fn accepts_valid_identity_headers() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let request = authenticated_request("test-cluster", 1);

                let result = interceptor.call(request);
                assert!(result.is_ok());
            }

            #[test]
            fn rejects_missing_cluster_id() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let mut request = authenticated_request("test-cluster", 1);
                request.metadata_mut().remove(HEADER_CLUSTER_ID);

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
            }

            #[test]
            fn rejects_cluster_id_mismatch() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let request = authenticated_request("WRONG-cluster", 1);

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
            }

            #[test]
            fn rejects_missing_node_id() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let mut request = authenticated_request("test-cluster", 1);
                request.metadata_mut().remove(HEADER_TARGET_NODE_ID);

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
            }

            #[test]
            fn rejects_node_id_mismatch() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let request = authenticated_request("test-cluster", 2);

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
            }

            #[test]
            fn rejects_malformed_node_id() {
                let mut interceptor = IdentityInterceptor::new(mock_identity("test-cluster", 1));
                let mut request = authenticated_request("test-cluster", 1);
                request
                    .metadata_mut()
                    .insert(HEADER_TARGET_NODE_ID, "not-a-number".parse().unwrap());

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
            }
        }

        mod inject_request {
            use super::*;

            #[test]
            fn inserts_identity_headers_correctly() {
                let mut request = Request::new(());
                let cluster_id = ClusterId::try_new("test-cluster").unwrap();
                let node_id = NodeId::new(42);

                IdentityInterceptor::inject_request(&mut request, &cluster_id, node_id).unwrap();

                assert_eq!(
                    request
                        .metadata()
                        .get(HEADER_CLUSTER_ID)
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "test-cluster"
                );
                assert_eq!(
                    request
                        .metadata()
                        .get(HEADER_TARGET_NODE_ID)
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "42"
                );
            }
        }
    }

    mod trace_interceptor {
        use super::*;

        mod helpers {
            use super::*;

            mod extract_request {
                use super::*;

                #[test]
                fn returns_some_when_extension_exists() {
                    let mut request = Request::new(());
                    let trace_id = TraceId::generate();
                    request.extensions_mut().insert(trace_id);

                    let result = TraceInterceptor::extract_request(&request).unwrap();
                    assert_eq!(result, trace_id);
                }

                #[test]
                fn returns_none_when_extension_missing() {
                    let request = Request::new(());
                    assert!(TraceInterceptor::extract_request(&request).is_none());
                }
            }

            mod require_trace_id {
                use super::*;

                #[test]
                fn should_return_trace_id_when_extension_exists() {
                    let mut request = Request::new(());
                    let trace_id = TraceId::generate();
                    request.extensions_mut().insert(trace_id);

                    let result = TraceInterceptor::require_trace_id(&request);
                    assert!(result.is_ok());
                    assert_eq!(result.unwrap(), trace_id);
                }

                #[test]
                fn should_return_failed_precondition_when_extension_missing() {
                    let request = Request::new(());
                    let result = TraceInterceptor::require_trace_id(&request);
                    assert!(result.is_err());
                    assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
                }
            }

            mod extract_response {
                use super::*;

                #[test]
                fn returns_some_when_metadata_exists() {
                    let mut response = Response::new(());
                    let trace_id = TraceId::generate();
                    let _ = TraceInterceptor::inject_response(&mut response, trace_id);

                    let result = TraceInterceptor::extract_response(&response).unwrap();
                    assert_eq!(result, trace_id);
                }

                #[test]
                fn returns_none_when_metadata_missing() {
                    let response = Response::new(());
                    assert!(TraceInterceptor::extract_response(&response).is_none());
                }
            }

            mod inject_request {
                use super::*;

                #[test]
                fn inserts_header_when_valid_id_provided() {
                    let mut request = Request::new(());
                    let trace_id = TraceId::generate();
                    let _ = TraceInterceptor::inject_request(&mut request, trace_id);

                    let header_val = request.metadata().get(HEADER_TRACE_ID).unwrap();
                    assert_eq!(header_val.to_str().unwrap(), trace_id.to_string());
                }
            }

            mod inject_response {
                use super::*;

                #[test]
                fn inserts_header_when_valid_id_provided() {
                    let mut response = Response::new(());
                    let trace_id = TraceId::generate();
                    let _ = TraceInterceptor::inject_response(&mut response, trace_id);

                    let header_val = response.metadata().get(HEADER_TRACE_ID).unwrap();
                    assert_eq!(header_val.to_str().unwrap(), trace_id.to_string());
                }
            }
        }

        mod call {
            use super::*;

            #[test]
            fn extracts_trace_id_in_propagative_mode() {
                let mut interceptor = TraceInterceptor::propagative();
                let mut request = Request::new(());
                let expected_trace = TraceId::generate();
                let _ = TraceInterceptor::inject_request(&mut request, expected_trace);

                let result = interceptor.call(request).unwrap();
                let extracted = TraceInterceptor::require_trace_id(&result).unwrap();
                assert_eq!(extracted, expected_trace);
            }

            #[test]
            fn generates_new_trace_id_in_authoritative_mode() {
                let mut interceptor = TraceInterceptor::authoritative();
                let request = Request::new(());

                let result = interceptor.call(request).unwrap();
                assert!(TraceInterceptor::require_trace_id(&result).is_ok());
            }

            #[test]
            fn authoritative_mode_ignores_client_provided_trace_id() {
                let mut interceptor = TraceInterceptor::authoritative();
                let mut request = Request::new(());
                let client_trace = TraceId::generate();
                let _ = TraceInterceptor::inject_request(&mut request, client_trace);

                let result = interceptor.call(request).unwrap();
                let assigned = TraceInterceptor::require_trace_id(&result).unwrap();
                assert_ne!(assigned, client_trace);
            }

            #[test]
            fn returns_error_when_header_is_missing_in_propagative_mode() {
                let mut interceptor = TraceInterceptor::propagative();
                let request = Request::new(());

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
            }

            #[test]
            fn returns_error_when_header_is_malformed_in_propagative_mode() {
                let mut interceptor = TraceInterceptor::propagative();
                let mut request = Request::new(());
                request
                    .metadata_mut()
                    .insert(HEADER_TRACE_ID, "invalid-uuid".parse().unwrap());

                let result = interceptor.call(request);
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
            }

            #[test]
            fn extract_response_is_strict() {
                let trace_id = TraceId::generate();
                let mut response = Response::new(());

                // 1. Success: Matching ID present
                let _ = TraceInterceptor::inject_response(&mut response, trace_id);
                assert_eq!(
                    TraceInterceptor::extract_response(&response).unwrap(),
                    trace_id
                );

                // 2. Failure: Metadata missing
                let empty_resp = Response::new(());
                assert!(TraceInterceptor::extract_response(&empty_resp).is_none());

                // 3. Failure: Malformed metadata
                let mut malformed = Response::new(());
                malformed
                    .metadata_mut()
                    .insert(HEADER_TRACE_ID, "not-a-uuid".parse().unwrap());
                assert!(TraceInterceptor::extract_response(&malformed).is_none());
            }
        }
    }
}
