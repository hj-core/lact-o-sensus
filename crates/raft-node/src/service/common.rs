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
            return Err(Status::invalid_argument("Cluster ID mismatch"));
        }
        Ok(())
    }

    /// Centralized health check for the Type-State engine.
    fn check_state_health(&self, state: &RaftNodeState) -> Result<(), Status> {
        if let RaftNodeState::Poisoned = state {
            return Err(Status::internal(
                "Node is in an unrecoverable state due to a failed transition",
            ));
        }
        Ok(())
    }
}
