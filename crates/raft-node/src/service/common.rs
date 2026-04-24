use std::sync::Arc;

pub use common::rpc::HEADER_CLUSTER_ID;
pub use common::rpc::HEADER_TARGET_NODE_ID;
use common::types::NodeIdentity;
use tonic::Status;
use tracing::error;

use crate::node::RaftNodeState;

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
                let msg = format!(
                    "CRITICAL: Identity divergence detected! ServiceIdentity='{:?}' \
                     EngineIdentity='{:?}'",
                    self.identity(),
                    engine_identity
                );
                error!("{}", msg);
                panic!("{}", msg);
            }
            Err(_) => {
                let msg = "CRITICAL: Node is in a poisoned state! Triggering Halt Mandate.";
                error!("{}", msg);
                panic!("{}", msg);
            }
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
