use std::sync::Arc;

use tonic::Status;
use tracing::warn;

use crate::identity::NodeIdentity;
use crate::node::RaftNodeState;

/// Shared trait for gRPC services to enforce cluster identity and node
/// integrity.
pub trait ServiceState {
    fn identity_arc(&self) -> &Arc<NodeIdentity>;

    /// Helper to get the underlying NodeIdentity reference.
    fn identity(&self) -> &NodeIdentity {
        self.identity_arc()
    }

    /// Helper to get the cluster ID as a borrowed string.
    fn cluster_id_as_str(&self) -> &str {
        self.identity().cluster_id().as_str()
    }

    /// Centralized Identity Guard (ADR 004).
    fn verify_identity(&self, cluster_id: &str) -> Result<(), Status> {
        if cluster_id != self.cluster_id_as_str() {
            warn!("Rejecting request from mismatching cluster: {}", cluster_id);
            return Err(self.invalid_cluster_id_status(cluster_id));
        }
        Ok(())
    }

    /// Centralized integrity check for the Type-State engine.
    ///
    /// This ensures the node is neither poisoned nor in an inconsistent
    /// identity state before proceeding with an operation.
    fn verify_node_integrity(&self, state: &RaftNodeState) -> Result<(), Status> {
        if let RaftNodeState::Poisoned = state {
            return Err(self.poisoned_status());
        }

        // Hard Guard: Ensure Dispatcher Identity is consistent with Node State.
        self.verify_consistency(state)?;

        Ok(())
    }

    /// Hard Guard: Verifies that the dispatcher's identity matches the node
    /// state's identity. Uses pointer equality as a fast-path before
    /// falling back to content comparison.
    fn verify_consistency(&self, state: &RaftNodeState) -> Result<(), Status> {
        let state_identity = state.identity_arc().map_err(|_| self.poisoned_status())?;

        // Fast Path: Pointer Equality
        if Arc::ptr_eq(self.identity_arc(), state_identity) {
            return Ok(());
        }

        // Slow Path: Content Comparison (NodeId and ClusterId)
        if self.identity().node_id() != state_identity.node_id()
            || self.identity().cluster_id() != state_identity.cluster_id()
        {
            tracing::error!(
                "CRITICAL IDENTITY DIVERGENCE: Dispatcher ({:?}) vs State ({:?})",
                self.identity(),
                state_identity
            );
            return Err(Status::internal("Internal Identity Mismatch"));
        }

        Ok(())
    }

    /// Returns a standard gRPC Internal status for a poisoned node.
    fn poisoned_status(&self) -> Status {
        Status::internal("Node is in an unrecoverable state due to a failed transition")
    }

    /// Returns a standard gRPC InvalidArgument status for a malformed NodeId.
    fn invalid_node_id_status(&self, id: &str) -> Status {
        Status::invalid_argument(format!(
            "'{}' is not a valid NodeId (must be a non-zero 64-bit integer)",
            id
        ))
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
