use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::consensus_service_client::ConsensusServiceClient;
use common::types::NodeId;
use thiserror::Error;
use tonic::transport::Channel;

use crate::identity::NodeIdentity;

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
    identity: Arc<NodeIdentity>,
    /// Pre-populated cache of peer connections.
    peers: HashMap<NodeId, PeerConnection>,
    /// The default timeout for consensus RPCs, pulled from config.
    default_rpc_timeout: Duration,
}

impl PeerManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        peer_map: &HashMap<NodeId, String>,
        default_rpc_timeout: Duration,
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

        Ok(Self {
            identity,
            peers,
            default_rpc_timeout,
        })
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
    ) -> Result<ConsensusServiceClient<Channel>, PeerError> {
        let peer = self
            .peers
            .get(&node_id)
            .ok_or(PeerError::NodeNotFound(node_id))?;

        // Cloning a Channel is cheap as it is an Arc-wrapped connection pool.
        Ok(ConsensusServiceClient::new(peer.channel.clone()))
    }

    /// Returns the network address (URL) for a specific peer.
    pub fn get_address(&self, node_id: NodeId) -> Result<String, PeerError> {
        self.peers
            .get(&node_id)
            .map(|p| p.address.clone())
            .ok_or(PeerError::NodeNotFound(node_id))
    }

    /// Returns the configured default timeout for consensus RPCs.
    pub fn default_rpc_timeout(&self) -> Duration {
        self.default_rpc_timeout
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

    fn mock_identity() -> Arc<NodeIdentity> {
        let id = NodeIdentity::new(ClusterId::try_new("test-cluster").unwrap(), NodeId::new(1));
        Arc::new(id)
    }

    mod peer_manager_get_client {
        use super::*;

        #[tokio::test]
        async fn returns_client_when_id_exists() {
            let mut peers = HashMap::new();
            peers.insert(NodeId::new(2), "http://127.0.0.1:50052".to_string());

            let manager =
                PeerManager::new(mock_identity(), &peers, Duration::from_millis(40)).unwrap();
            let result = manager.get_client(NodeId::new(2));

            assert!(result.is_ok());
        }

        #[test]
        fn returns_err_when_id_missing() {
            let manager =
                PeerManager::new(mock_identity(), &HashMap::new(), Duration::from_millis(40))
                    .unwrap();
            let result = manager.get_client(NodeId::new(99));

            assert!(matches!(result, Err(PeerError::NodeNotFound(_))));
        }
    }

    mod peer_manager_get_address {
        use super::*;

        #[tokio::test]
        async fn returns_address_when_id_exists() {
            let mut peers = HashMap::new();
            peers.insert(NodeId::new(2), "http://127.0.0.1:50052".to_string());

            let manager =
                PeerManager::new(mock_identity(), &peers, Duration::from_millis(40)).unwrap();
            let result = manager.get_address(NodeId::new(2));

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "http://127.0.0.1:50052");
        }
    }
}
