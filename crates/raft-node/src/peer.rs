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

/// Manages outbound gRPC connections to other nodes in the cluster.
///
/// Peer connections are lazy-initialized using `connect_lazy` to ensure
/// that the node can start even if some peers are temporarily unreachable.
#[derive(Debug)]
pub struct PeerManager {
    #[allow(dead_code)]
    identity: Arc<NodeIdentity>,
    peers: HashMap<NodeId, String>, // Mapping of node_id to URL
    default_rpc_timeout: Duration,
}

impl PeerManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        peer_map: &HashMap<NodeId, std::net::SocketAddr>,
        default_rpc_timeout: Duration,
    ) -> Self {
        let peers = peer_map
            .iter()
            .map(|(id, addr)| (*id, format!("http://{}", addr)))
            .collect();

        Self {
            identity,
            peers,
            default_rpc_timeout,
        }
    }

    /// Creates a lazy-initialized client for a specific peer.
    ///
    /// NOTE: We do not enforce the `rpc_timeout` at the Channel level here.
    /// This allows individual requests to exceed the short timeout if
    /// necessary. Callers should apply the timeout to the `tonic::Request`.
    pub fn get_client(
        &self,
        node_id: NodeId,
    ) -> Result<ConsensusServiceClient<Channel>, PeerError> {
        let addr = self
            .peers
            .get(&node_id)
            .ok_or(PeerError::NodeNotFound(node_id))?;

        // Channel::from_shared verifies the URL format, but connect_lazy
        // defers the actual TCP handshake until the first request.
        let channel = Channel::from_shared(addr.clone())
            .map_err(|_| PeerError::InvalidUri {
                node_id,
                uri: addr.clone(),
            })?
            .connect_lazy();

        Ok(ConsensusServiceClient::new(channel))
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
    use std::net::SocketAddr;

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
            peers.insert(
                NodeId::new(2),
                "127.0.0.1:50052".parse::<SocketAddr>().unwrap(),
            );

            let manager = PeerManager::new(mock_identity(), &peers, Duration::from_millis(40));
            let result = manager.get_client(NodeId::new(2));

            assert!(result.is_ok());
        }

        #[test]
        fn returns_err_when_id_missing() {
            let manager =
                PeerManager::new(mock_identity(), &HashMap::new(), Duration::from_millis(40));
            let result = manager.get_client(NodeId::new(99));

            assert!(matches!(result, Err(PeerError::NodeNotFound(_))));
        }
    }
}
