use std::collections::HashMap;
use std::sync::Arc;

use common::proto::v1::raft::consensus_service_client::ConsensusServiceClient;
use common::types::NodeId;
use common::types::NodeIdentity;
use thiserror::Error;
use tonic::Request;
use tonic::Status;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::service::common::HEADER_CLUSTER_ID;
use crate::service::common::HEADER_TARGET_NODE_ID;

#[derive(Debug, Error)]
pub enum PeerError {
    #[error("Node ID {0} not found in peer map")]
    NodeNotFound(NodeId),

    #[error("Invalid URI for node {node_id} ('{uri}')")]
    InvalidUri { node_id: NodeId, uri: String },
}

/// Represents a persistent connection to a peer node.
#[derive(Debug, Clone)]
struct PeerConnection {
    channel: Channel,
    address: String,
}

/// Manages outbound gRPC connections to other nodes in the cluster.
///
/// Peer connections are lazy-initialized using `connect_lazy`. This ensures
/// that the node can start even if some peers are temporarily unreachable.
///
/// In this phase, the peer list is static and established at startup.
#[derive(Debug)]
pub struct PeerManager {
    /// Local node identity for outbound header injection.
    identity: Arc<NodeIdentity>,
    /// Pre-populated cache of peer connections.
    peers: HashMap<NodeId, PeerConnection>,
}

impl PeerManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        peer_map: &HashMap<NodeId, String>,
    ) -> Result<Self, PeerError> {
        let mut peers = HashMap::new();

        for (id, uri) in peer_map {
            let channel = Channel::from_shared(uri.clone())
                .map_err(|_| PeerError::InvalidUri {
                    node_id: *id,
                    uri: uri.clone(),
                })?
                .connect_lazy();

            peers.insert(
                *id,
                PeerConnection {
                    channel,
                    address: uri.clone(),
                },
            );
        }

        Ok(Self { identity, peers })
    }

    /// Returns a client for a specific peer from the internal channel cache.
    ///
    /// NOTE: We do not enforce the `rpc_timeout` at the transport (Channel)
    /// level. This allows the same connection to be used for both
    /// low-latency consensus RPCs and potentially high-latency operations
    /// (like snapshot transfers) by applying specific timeouts to the
    /// `tonic::Request` itself.
    pub fn get_client(
        &self,
        node_id: NodeId,
    ) -> Result<
        ConsensusServiceClient<
            InterceptedService<
                Channel,
                impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone,
            >,
        >,
        PeerError,
    > {
        let peer = self
            .peers
            .get(&node_id)
            .ok_or(PeerError::NodeNotFound(node_id))?;

        let cluster_id = self.identity.cluster_id().clone();
        let target_node_id = node_id;

        // Interceptor to inject identity headers for cluster isolation.
        let interceptor = move |mut req: Request<()>| {
            req.metadata_mut().insert(
                HEADER_CLUSTER_ID,
                cluster_id.as_str().parse().map_err(|_| {
                    Status::internal("Failed to parse cluster_id for outbound header")
                })?,
            );
            req.metadata_mut().insert(
                HEADER_TARGET_NODE_ID,
                target_node_id.to_string().parse().map_err(|_| {
                    Status::internal("Failed to parse target_node_id for outbound header")
                })?,
            );
            Ok(req)
        };

        // Cloning a Channel is cheap as it is an Arc-wrapped connection pool.
        Ok(ConsensusServiceClient::with_interceptor(
            peer.channel.clone(),
            interceptor,
        ))
    }

    /// Returns the network address (URL) for a specific peer.
    pub fn get_address(&self, node_id: NodeId) -> Result<String, PeerError> {
        self.peers
            .get(&node_id)
            .map(|p| p.address.clone())
            .ok_or(PeerError::NodeNotFound(node_id))
    }

    /// Returns a list of all peer IDs configured for this cluster.
    pub fn peer_ids(&self) -> Vec<NodeId> {
        self.peers.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;

    use super::*;

    mod get_client {
        use super::*;

        fn mock_identity() -> Arc<NodeIdentity> {
            Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::new(1),
            ))
        }

        #[tokio::test]
        async fn returns_intercepted_client_when_node_exists() {
            let mut peers = HashMap::new();
            peers.insert(NodeId::new(2), "http://127.0.0.1:50052".to_string());

            let manager = PeerManager::new(mock_identity(), &peers).unwrap();
            let result = manager.get_client(NodeId::new(2));

            assert!(result.is_ok());
        }

        #[test]
        fn returns_error_when_node_id_is_missing() {
            let manager = PeerManager::new(mock_identity(), &HashMap::new()).unwrap();
            let result = manager.get_client(NodeId::new(99));

            assert!(matches!(result, Err(PeerError::NodeNotFound(_))));
        }
    }

    mod get_address {
        use super::*;

        fn mock_identity() -> Arc<NodeIdentity> {
            Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::new(1),
            ))
        }

        #[tokio::test]
        async fn returns_network_address_when_node_exists() {
            let mut peers = HashMap::new();
            peers.insert(NodeId::new(2), "http://127.0.0.1:50052".to_string());

            let manager = PeerManager::new(mock_identity(), &peers).unwrap();
            let result = manager.get_address(NodeId::new(2));

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "http://127.0.0.1:50052");
        }
    }
}
