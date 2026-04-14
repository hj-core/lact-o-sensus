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
/// Peer connections are lazy-initialized using `connect_lazy`. This ensures
/// that the node can start even if some peers are temporarily unreachable.
///
/// In this phase, the peer list is static and established at startup.
#[derive(Debug)]
pub struct PeerManager {
    identity: Arc<NodeIdentity>,
    /// Pre-populated cache of gRPC channels.
    channels: HashMap<NodeId, Channel>,
    /// The default timeout for consensus RPCs, pulled from config.
    default_rpc_timeout: Duration,
}

impl PeerManager {
    pub fn new(
        identity: Arc<NodeIdentity>,
        peer_map: &HashMap<NodeId, std::net::SocketAddr>,
        default_rpc_timeout: Duration,
    ) -> Result<Self, PeerError> {
        let mut channels = HashMap::new();

        for (id, addr) in peer_map {
            let uri = format!("http://{}", addr);
            let channel = Channel::from_shared(uri.clone())
                .map_err(|_| PeerError::InvalidUri {
                    node_id: *id,
                    uri: uri.clone(),
                })?
                .connect_lazy();

            channels.insert(*id, channel);
        }

        Ok(Self {
            identity,
            channels,
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
        let channel = self
            .channels
            .get(&node_id)
            .ok_or(PeerError::NodeNotFound(node_id))?;

        // Cloning a Channel is cheap as it is an Arc-wrapped connection pool.
        Ok(ConsensusServiceClient::new(channel.clone()))
    }

    /// Returns the configured default timeout for consensus RPCs.
    pub fn default_rpc_timeout(&self) -> Duration {
        self.default_rpc_timeout
    }

    /// Returns a list of all peer IDs configured for this cluster.
    pub fn peer_ids(&self) -> Vec<NodeId> {
        self.channels.keys().copied().collect()
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
}
