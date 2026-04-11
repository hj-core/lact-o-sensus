use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use common::proto::v1::consensus_service_client::ConsensusServiceClient;
use common::types::NodeId;
use tonic::transport::Channel;

use crate::identity::NodeIdentity;

/// Manages outbound gRPC connections to other nodes in the cluster.
///
/// Peer connections are lazy-initialized using `connect_lazy` to ensure
/// that the node can start even if some peers are temporarily unreachable.
#[derive(Debug)]
pub struct PeerManager {
    #[allow(dead_code)]
    identity: Arc<NodeIdentity>,
    peers: HashMap<NodeId, String>, // Mapping of node_id to URL
}

impl PeerManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        peer_map: &HashMap<NodeId, std::net::SocketAddr>,
    ) -> Self {
        let peers = peer_map
            .iter()
            .map(|(id, addr)| (*id, format!("http://{}", addr)))
            .collect();

        Self { identity, peers }
    }

    /// Creates a lazy-initialized client for a specific peer.
    ///
    /// Per our strategy, this does not perform a handshake immediately,
    /// allowing the cluster to start up even if some nodes are offline.
    pub fn get_client(&self, node_id: NodeId) -> Result<ConsensusServiceClient<Channel>> {
        let addr = self
            .peers
            .get(&node_id)
            .with_context(|| format!("Node ID {} not found in peer map", node_id))?;

        // Channel::from_shared verifies the URL format, but connect_lazy
        // defers the actual TCP handshake until the first request.
        let channel = Channel::from_shared(addr.clone())?.connect_lazy();

        Ok(ConsensusServiceClient::new(channel))
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
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            1.into(),
        ))
    }

    mod get_client {
        use super::*;

        #[tokio::test]
        async fn returns_client_when_id_exists() {
            let mut peers = HashMap::new();
            peers.insert(
                NodeId::new(2),
                "127.0.0.1:50052".parse::<SocketAddr>().unwrap(),
            );

            let manager = PeerManager::new(mock_identity(), &peers);
            let result = manager.get_client(NodeId::new(2));

            assert!(result.is_ok());
        }

        #[test]
        fn returns_err_when_id_missing() {
            let manager = PeerManager::new(mock_identity(), &HashMap::new());
            let result = manager.get_client(NodeId::new(99));

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("not found"));
        }
    }
}
