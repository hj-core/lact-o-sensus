use std::sync::Arc;

use common::types::NodeId;
use tonic::Request;
use tonic::Status;
use tracing::error;
use tracing::warn;

use crate::identity::NodeIdentity;
use crate::node::RaftNodeState;

/// Metadata header key for the cluster identifier.
pub const HEADER_CLUSTER_ID: &str = "x-cluster-id";
/// Metadata header key for the target node identifier.
pub const HEADER_TARGET_NODE_ID: &str = "x-target-node-id";

/// Shared trait for gRPC services to enforce cluster identity and node
/// integrity.
pub trait ServiceState: Send + Sync {
    /// Returns a reference to the node's verified identity.
    fn identity_arc(&self) -> &Arc<NodeIdentity>;

    /// Returns a reference to the node's verified identity.
    fn identity(&self) -> &NodeIdentity {
        self.identity_arc()
    }

    /// Verifies that the node engine is healthy and matches the service
    /// identity.
    ///
    /// Enforces ptr_eq to ensure a Single Source of Truth for identity.
    fn verify_node_integrity(&self, node: &RaftNodeState) -> Result<(), Status> {
        match node.identity_arc() {
            Ok(engine_identity) if Arc::ptr_eq(engine_identity, self.identity_arc()) => Ok(()),
            Ok(engine_identity) => {
                // Internal invariant violation: Service and Engine identity must match.
                error!(
                    "CRITICAL: Identity divergence detected! ServiceIdentity='{:?}' \
                     EngineIdentity='{:?}'",
                    self.identity(),
                    engine_identity
                );
                Err(Status::internal("Internal identity mismatch"))
            }
            Err(_) => Err(self.poisoned_status()),
        }
    }

    /// Returns a standard gRPC Internal status for poisoned nodes.
    fn poisoned_status(&self) -> Status {
        Status::internal("Node is in a poisoned state following a fatal transition failure")
    }

    /// Returns a standard gRPC InvalidArgument status for invalid Node IDs.
    fn invalid_node_id_status(&self, input: &str) -> Status {
        Status::invalid_argument(format!("Invalid NodeId format: '{}'", input))
    }
}

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
}

impl tonic::service::Interceptor for IdentityInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        // 1. Verify Cluster ID (Mandatory Boundary Check)
        let cluster_id_header = request
            .metadata()
            .get(HEADER_CLUSTER_ID)
            .ok_or_else(|| {
                Status::unauthenticated(format!("Missing mandatory {} header", HEADER_CLUSTER_ID))
            })?
            .to_str()
            .map_err(|_| Status::unauthenticated("Invalid cluster ID encoding"))?;

        if cluster_id_header != self.identity.cluster_id().as_str() {
            // Logged as WARN to prevent Log Flooding DoS.
            warn!(
                "ISOLATION MISMATCH: Cluster ID mismatch (expected {}, got {})",
                self.identity.cluster_id(),
                cluster_id_header
            );
            return Err(Status::unauthenticated("Cluster identity mismatch"));
        }

        // 2. Verify Target Node ID (Mandatory Routing Check)
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
            // Logged as WARN to prevent Log Flooding DoS.
            warn!(
                "ROUTING MISMATCH: Target node mismatch (expected {}, got {})",
                self.identity.node_id(),
                target_node_id
            );
            return Err(Status::unauthenticated("Target node identity mismatch"));
        }

        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    mod call {
        use std::sync::Arc;

        use common::types::ClusterId;
        use common::types::NodeId;
        use tonic::Request;
        use tonic::service::Interceptor;

        use super::super::*;
        use crate::identity::NodeIdentity;

        fn mock_identity(cluster: &str, node_id: u64) -> Arc<NodeIdentity> {
            Arc::new(NodeIdentity::new(
                ClusterId::try_new(cluster).unwrap(),
                NodeId::new(node_id),
            ))
        }

        #[test]
        fn accepts_request_when_identity_headers_are_valid() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_CLUSTER_ID, "test-cluster".parse().unwrap());
            request
                .metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, "1".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_ok());
        }

        #[test]
        fn returns_unauthenticated_when_cluster_id_is_missing() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, "1".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }

        #[test]
        fn returns_unauthenticated_when_cluster_id_mismatches() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_CLUSTER_ID, "wrong-cluster".parse().unwrap());
            request
                .metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, "1".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }

        #[test]
        fn returns_unauthenticated_when_node_id_is_missing() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_CLUSTER_ID, "test-cluster".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }

        #[test]
        fn returns_unauthenticated_when_node_id_mismatches() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_CLUSTER_ID, "test-cluster".parse().unwrap());
            request
                .metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, "2".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }

        #[test]
        fn returns_unauthenticated_when_node_id_format_is_invalid() {
            let identity = mock_identity("test-cluster", 1);
            let mut interceptor = IdentityInterceptor::new(identity);

            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert(HEADER_CLUSTER_ID, "test-cluster".parse().unwrap());
            request
                .metadata_mut()
                .insert(HEADER_TARGET_NODE_ID, "not-a-number".parse().unwrap());

            let result = interceptor.call(request);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }
    }
}
