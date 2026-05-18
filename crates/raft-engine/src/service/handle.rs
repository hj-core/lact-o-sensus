use std::sync::Arc;

use async_trait::async_trait;
use common::raft_api::ConsensusHandle;
use common::raft_api::ConsensusStatus;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::errors::ConsensusError;
use common::types::errors::NodeError;

use crate::engine::LogicalNode;
use crate::engine::NodeRole;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;

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
impl ConsensusHandle for LocalRaftHandle {
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
        let mut guard = self.state.write().await;
        guard.propose(data).map_err(|e| match e {
            NodeError::NotLeader { .. } => ConsensusError::NotLeader,
            _ => ConsensusError::Internal(e.to_string()),
        })
    }

    async fn await_commit(&self, index: LogIndex) -> Result<(), ConsensusError> {
        let mut progress_rx = self.state.subscribe();

        loop {
            // Check condition first (prevents missing updates before subscription)
            {
                let guard = self.state.read().await;
                match &*guard {
                    LogicalNode::Leader(node) => {
                        if node.last_committed() >= index {
                            return Ok(());
                        }
                    }
                    LogicalNode::Poisoned => {
                        return Err(ConsensusError::Poisoned);
                    }
                    _ => {
                        return Err(ConsensusError::NotLeader);
                    }
                }
            }

            // Park until something changes (LogIndex OR Term/Role/Poison)
            if progress_rx.changed().await.is_err() {
                return Err(ConsensusError::Terminated);
            }
        }
    }

    async fn await_apply(&self, index: LogIndex) -> Result<(), ConsensusError> {
        let mut progress_rx = self.state.subscribe();

        loop {
            // Check condition first
            {
                let progress = progress_rx.borrow();
                if progress.role == NodeRole::Poisoned {
                    return Err(ConsensusError::Poisoned);
                }
                if progress.last_applied >= index {
                    return Ok(());
                }
            }

            // Park until something changes
            if progress_rx.changed().await.is_err() {
                return Err(ConsensusError::Terminated);
            }
        }
    }

    async fn consensus_status(&self) -> ConsensusStatus {
        let progress = *self.state.subscribe().borrow();
        let guard = self.state.read().await;
        let is_leader = matches!(&*guard, LogicalNode::Leader(_));
        let (leader_hint, rejection_reason) = self.calculate_redirection(&guard);

        ConsensusStatus {
            is_leader,
            last_committed: progress.last_committed,
            leader_hint,
            rejection_reason,
        }
    }

    async fn verify_leadership(&self) -> Result<(), ConsensusError> {
        let guard = self.state.read().await;
        match &*guard {
            LogicalNode::Leader(_) => {
                // TODO: Step 3 - Enhance with Quorum Heartbeat for strict linearizability.
                // Currently, this only performs a local authority check which is
                // vulnerable to stale reads in partitioned scenarios.
                Ok(())
            }
            LogicalNode::Poisoned => Err(ConsensusError::Poisoned),
            _ => Err(ConsensusError::NotLeader),
        }
    }
}

#[cfg(test)]
mod tests {
    use common::raft_api::StateMachine;
    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use common::types::Term;
    use common::types::errors::FsmError;

    use super::*;
    use crate::engine::Follower;
    use crate::engine::LogicalNode;
    use crate::node::RaftNode;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
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
        let storage = Arc::new(MemoryStorage::new());
        let node =
            LogicalNode::Follower(RaftNode::<Follower>::try_new(id.clone(), fsm, storage).unwrap());
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
                guard.into_candidate();
                guard.into_leader(vec![]);
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
            assert_eq!(result.unwrap_err(), ConsensusError::NotLeader);
        }

        #[tokio::test]
        async fn succeeds_when_leader() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![]);
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
                guard.into_candidate();
                guard.into_leader(vec![]);
            }

            let index = handle.propose(vec![1]).await.unwrap();

            // Simulate commitment in background
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut guard = state_clone.write().await;
                if let LogicalNode::Leader(_) = &mut *guard {
                    guard.advance_last_committed(index).await;
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
                guard.into_candidate();
                guard.into_leader(vec![]);
            }

            let index = LogIndex::new(10); // A high index that won't be reached immediately

            // Simulate demotion in background
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut guard = state_clone.write().await;
                guard.into_follower(Term::new(2), None);
            });

            let result = handle.await_commit(index).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), ConsensusError::NotLeader);
        }
    }

    mod await_apply {
        use std::time::Duration;

        use common::proto::v1::raft::LogEntry;
        use tokio::time::sleep;

        use super::*;

        #[tokio::test]
        async fn succeeds_when_index_is_reached() {
            let (handle, state) = setup();
            let index = LogIndex::new(5);

            // Simulate FSM apply in background
            let state_clone = state.clone();
            tokio::spawn(async move {
                sleep(Duration::from_millis(10)).await;
                let mut guard = state_clone.write().await;

                // Advance last_committed (which triggers FSM apply and updates last_applied)
                // LogicalNode::advance_last_committed is role-agnostic.
                // We first need to ensure the log contains the entries we are committing.
                if let LogicalNode::Follower(node) = &mut *guard {
                    let mut entries = Vec::new();
                    for i in 1..=index.value() {
                        entries.push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
                    }
                    node.log_store().append_entries(entries).unwrap();
                }

                guard.advance_last_committed(index).await;
            });

            let result = handle.await_apply(index).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn succeeds_immediately_if_already_applied() {
            let (handle, state) = setup();
            let index = LogIndex::new(5);

            {
                let mut guard = state.write().await;
                // Prepare log and advance state
                if let LogicalNode::Follower(node) = &mut *guard {
                    let mut entries = Vec::new();
                    for i in 1..=index.value() {
                        entries.push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
                    }
                    node.log_store().append_entries(entries).unwrap();
                }
                guard.advance_last_committed(index).await;
            }

            let result = handle.await_apply(index).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn fails_if_node_becomes_poisoned() {
            let (handle, state) = setup();

            // Start waiting for a high index
            let wait_task =
                tokio::spawn(async move { handle.await_apply(LogIndex::new(100)).await });

            sleep(Duration::from_millis(10)).await;
            {
                let mut guard = state.write().await;
                *guard = LogicalNode::Poisoned;
            }

            let result = wait_task.await.unwrap();
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }
    }

    mod poison_safety {
        use super::*;

        #[tokio::test]
        async fn consensus_status_reports_poisoning() {
            let (handle, state) = setup();
            {
                // Manually transition to poisoned state
                let mut guard = state.write().await;
                *guard = LogicalNode::Poisoned;
            }

            let status = handle.consensus_status().await;
            assert!(!status.is_leader);
            assert_eq!(status.rejection_reason, "Node is in a poisoned state.");
        }

        #[tokio::test]
        async fn verify_leadership_returns_poisoned_error() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                *guard = LogicalNode::Poisoned;
            }

            let result = handle.verify_leadership().await;
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }

        #[tokio::test]
        async fn await_commit_fails_immediately_if_already_poisoned() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                *guard = LogicalNode::Poisoned;
            }

            let result = handle.await_commit(LogIndex::new(1)).await;
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }

        #[tokio::test]
        async fn await_commit_terminates_when_poisoned_during_wait() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![]);
            }

            // Start waiting for a high index
            let wait_task =
                tokio::spawn(async move { handle.await_commit(LogIndex::new(100)).await });

            // Simulate a fatal crash in a background task
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            {
                let mut guard = state.write().await;
                *guard = LogicalNode::Poisoned;
                // MutationGuard drop triggers the watch channel update
            }

            let result = wait_task.await.unwrap();
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }
    }
}
