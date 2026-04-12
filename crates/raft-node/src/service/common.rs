use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::Status;
use tracing::warn;

use crate::identity::NodeIdentity;
use crate::node::RaftNodeState;

/// Shared trait for gRPC services to enforce cluster identity and node health.
pub trait ServiceState {
    fn identity(&self) -> &NodeIdentity;
    fn state(&self) -> &Arc<RwLock<RaftNodeState>>;

    /// Helper to get the cluster ID as a borrowed string.
    fn cluster_id_as_str(&self) -> &str {
        self.identity().cluster_id().as_str()
    }

    /// Centralized Identity Guard (ADR 004).
    fn verify_cluster_id(&self, cluster_id: &str) -> Result<(), Status> {
        if cluster_id != self.cluster_id_as_str() {
            warn!("Rejecting request from mismatching cluster: {}", cluster_id);
            return Err(self.invalid_cluster_id_status(cluster_id));
        }
        Ok(())
    }

    /// Centralized health check for the Type-State engine.
    fn check_state_health(&self, state: &RaftNodeState) -> Result<(), Status> {
        if let RaftNodeState::Poisoned = state {
            return Err(self.poisoned_status());
        }
        Ok(())
    }

    /// Returns a standard gRPC Internal status for a poisoned node.
    fn poisoned_status(&self) -> Status {
        Status::internal("Node is in an unrecoverable state due to a failed transition")
    }

    /// Returns a standard gRPC InvalidArgument status for a malformed NodeId.
    fn invalid_node_id_status(&self, id: &str) -> Status {
        Status::invalid_argument(format!("'{}' is not a valid NodeId (must be u64)", id))
    }

    /// Returns a standard gRPC InvalidArgument status for a mismatching
    /// ClusterId.
    fn invalid_cluster_id_status(&self, cluster_id: &str) -> Status {
        // ADR 004 / Security: Do not leak the expected cluster_id to the outside world.
        // We log the details internally for debugging, but return an opaque error.
        Status::invalid_argument(format!(
            "Provided cluster_id '{}' is not authorized for this node",
            cluster_id
        ))
    }
}
