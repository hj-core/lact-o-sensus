use std::sync::Arc;

use async_trait::async_trait;
use common::raft_api::ConsensusStatus;
use common::raft_api::RaftHandle;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::SequenceId;
use tonic::Status;

use crate::engine::ConsensusError;
use crate::engine::LogicalNode;
use crate::peer::PeerManager;
use crate::state::ConsensusShell;

#[derive(Debug)]
pub struct LocalRaftHandle {
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
}

impl LocalRaftHandle {
    pub fn new(state: Arc<ConsensusShell>, peer_manager: Arc<PeerManager>) -> Self {
        Self {
            state,
            peer_manager,
        }
    }

    /// Determines the leader address and status message based on the current
    /// role.
    fn calculate_redirection(&self, node_state: &LogicalNode) -> (String, String) {
        match node_state {
            LogicalNode::Follower(node) => self.follower_redirection(node.state().leader_id()),
            LogicalNode::Candidate(_) => (
                String::new(),
                "Election in progress. No leader established.".to_string(),
            ),
            LogicalNode::Leader(_) => (String::new(), String::new()),
            LogicalNode::Poisoned => (String::new(), "Node is in a poisoned state.".to_string()),
        }
    }

    /// Helper to resolve the leader's network address for a follower.
    fn follower_redirection(&self, leader_id: Option<NodeId>) -> (String, String) {
        match leader_id {
            Some(id) => match self.peer_manager.get_address(id) {
                Ok(addr) => (
                    addr,
                    format!("Node is a Follower. Retry with Leader at NodeID {}.", id),
                ),
                Err(_) => (
                    String::new(),
                    format!("Follower of {}, but address is missing.", id),
                ),
            },
            None => (
                String::new(),
                "Node is a Follower; leader is unknown.".to_string(),
            ),
        }
    }
}

#[async_trait]
impl RaftHandle for LocalRaftHandle {
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, Status> {
        let mut guard = self.state.write().await;
        match guard.propose(data) {
            Ok(idx) => Ok(idx),
            Err(ConsensusError::NotLeader) => Err(Status::failed_precondition("Not the leader")),
        }
    }

    async fn await_commit(&self, index: LogIndex) -> Result<(), Status> {
        let mut progress_rx = self.state.subscribe();

        loop {
            // Check condition first (prevents missing updates before subscription)
            {
                let guard = self.state.read().await;
                match &*guard {
                    LogicalNode::Leader(node) => {
                        if node.commit_index() >= index {
                            return Ok(());
                        }
                    }
                    _ => {
                        return Err(Status::unavailable(
                            "Leadership lost while waiting for quorum",
                        ));
                    }
                }
            }

            // Park until something changes (LogIndex OR Term/Role)
            if progress_rx.changed().await.is_err() {
                return Err(Status::internal("Consensus engine terminated"));
            }
        }
    }

    async fn consensus_status(&self) -> ConsensusStatus {
        let guard = self.state.read().await;
        let is_leader = matches!(&*guard, LogicalNode::Leader(_));
        let (leader_hint, rejection_reason) = self.calculate_redirection(&*guard);

        ConsensusStatus {
            is_leader,
            leader_hint,
            rejection_reason,
        }
    }

    async fn check_session(
        &self,
        _client_id: &ClientId,
        _sequence_id: SequenceId,
    ) -> Result<Option<LogIndex>, Status> {
        // TODO: Phase 9/10 - Wire to LactoStore Session Table (sled)
        Ok(None)
    }

    async fn verify_leadership(&self) -> Result<(), Status> {
        let guard = self.state.read().await;
        if matches!(&*guard, LogicalNode::Leader(_)) {
            // TODO: Step 3 - Enhance with Quorum Heartbeat for strict linearizability
            Ok(())
        } else {
            let (hint, reason) = self.calculate_redirection(&*guard);
            Err(Status::failed_precondition(format!(
                "Not the leader. Hint: {} ({})",
                hint, reason
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use common::types::Term;

    use super::*;
    use crate::engine::Follower;
    use crate::engine::LogicalNode;
    use crate::fsm::StateMachine;
    use crate::node::RaftNode;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Status> {
            Ok(())
        }
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn setup() -> (LocalRaftHandle, Arc<ConsensusShell>) {
        let id = mock_identity();
        let fsm = Arc::new(MockFsm);
        let node = LogicalNode::Follower(RaftNode::<Follower>::new(id.node_id(), fsm));
        let state = Arc::new(ConsensusShell::new(node));
        let peer_manager =
            Arc::new(PeerManager::new(id, &std::collections::HashMap::new()).unwrap());
        (LocalRaftHandle::new(state.clone(), peer_manager), state)
    }

    mod consensus_status {
        use super::*;

        #[tokio::test]
        async fn reports_correctly_for_follower_without_leader() {
            let (handle, _) = setup();
            let status = handle.consensus_status().await;

            assert!(!status.is_leader);
            assert!(status.leader_hint.is_empty());
            assert!(status.rejection_reason.contains("leader is unknown"));
        }

        #[tokio::test]
        async fn reports_correctly_for_leader() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    LogicalNode::Follower(n) => {
                        LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                    }
                    _ => panic!("Expected follower"),
                });
            }
            let status = handle.consensus_status().await;

            assert!(status.is_leader);
            assert!(status.leader_hint.is_empty());
            assert!(status.rejection_reason.is_empty());
        }
    }

    mod propose {
        use super::*;

        #[tokio::test]
        async fn fails_when_not_leader() {
            let (handle, _) = setup();
            let result = handle.propose(vec![1, 2, 3]).await;

            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        }

        #[tokio::test]
        async fn succeeds_when_leader() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    LogicalNode::Follower(n) => {
                        LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                    }
                    _ => panic!("Expected follower"),
                });
            }

            let result = handle.propose(vec![1, 2, 3]).await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap().value(), 1);
        }
    }

    mod await_commit {
        use std::time::Duration;

        use super::*;

        #[tokio::test]
        async fn succeeds_when_index_is_reached() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    LogicalNode::Follower(n) => {
                        LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                    }
                    _ => panic!("Expected follower"),
                });
            }

            let index = handle.propose(vec![1]).await.unwrap();

            // Simulate commitment in background
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut guard = state_clone.write().await;
                if let LogicalNode::Leader(node) = &mut *guard {
                    node.set_commit_index(index).await;
                }
            });

            let result = handle.await_commit(index).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn fails_when_leadership_is_lost() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    LogicalNode::Follower(n) => {
                        LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                    }
                    _ => panic!("Expected follower"),
                });
            }

            let index = LogIndex::new(10); // A high index that won't be reached immediately

            // Simulate demotion in background
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut guard = state_clone.write().await;
                guard.transition(|old| old.into_follower(Term::new(2), None));
            });

            let result = handle.await_commit(index).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
        }
    }
}
