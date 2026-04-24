use std::sync::Arc;

use tonic::Request;
use tonic::Status;
use tonic::service::Interceptor;
use tracing::warn;

use crate::types::NodeId;
use crate::types::NodeIdentity;

/// Metadata header key for the cluster identifier.
pub const HEADER_CLUSTER_ID: &str = "x-cluster-id";
/// Metadata header key for the target node identifier.
pub const HEADER_TARGET_NODE_ID: &str = "x-target-node-id";

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
                "ISOLATION MISMATCH: Cluster ID mismatch (expected {}, got {})",
                self.identity.cluster_id(),
                cluster_id_header
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
                "ROUTING MISMATCH: Target node mismatch (expected {}, got {})",
                self.identity.node_id(),
                target_node_id
            );
            return Err(Status::unauthenticated("Target node identity mismatch"));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ClusterId;

    fn mock_identity(cluster: &str, node_id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new(cluster).unwrap(),
            NodeId::new(node_id),
        ))
    }

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
}
