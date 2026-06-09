//! Local API Handle (The "Execution Shell").
//!
//! This module implements the `ConsensusHandle` trait, providing the primary
//! API for the gateway and application layers to interact with the consensus
//! engine. It acts as the "Execution Shell" (ADR 009) that translates
//! high-level intents into consensus operations.

use std::sync::Arc;

use async_trait::async_trait;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::errors::ConsensusError;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tracing::Instrument;
use tracing::error;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;

use crate::config::Config;
use crate::consensus_api::ConsensusAuthority;
use crate::consensus_api::ConsensusHandle;
use crate::engine::NodeRole;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;

/// Authoritative handle for local Raft operations.
///
/// This handle provides a thread-safe, reactive interface for proposing
/// mutations and awaiting state transitions. It utilizes a `watch` channel to
/// monitor consensus progress without excessive lock contention.
#[derive(Debug)]
pub struct LocalRaftHandle<S: StateMachine> {
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
}

impl<S: StateMachine> LocalRaftHandle<S> {
    /// Creates a new execution shell for the given consensus state.
    pub fn new(
        config: Arc<Config>,
        state: Arc<ConsensusShell<S>>,
        peer_manager: Arc<PeerManager>,
    ) -> Self {
        Self {
            config,
            state,
            peer_manager,
        }
    }

    /// Determines the leader address and status message based on the current
    /// role.
    fn calculate_redirection(
        &self,
        role: NodeRole,
        leader_hint: Option<NodeId>,
    ) -> (String, String) {
        match role {
            NodeRole::Follower => self.follower_redirection(leader_hint),
            NodeRole::PreCandidate => (
                String::new(),
                "Pre-election in progress. No leader established.".to_string(),
            ),
            NodeRole::Candidate => (
                String::new(),
                "Election in progress. No leader established.".to_string(),
            ),
            NodeRole::Leader => (String::new(), String::new()),
            NodeRole::Poisoned => (String::new(), "Node is in a poisoned state.".to_string()),
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
impl<S: StateMachine> ConsensusHandle for LocalRaftHandle<S> {
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "propose",
            data_len = data.len()
        );

        async {
            let mut guard = self.state.write().await;
            guard.propose(data).map_err(|e| match e {
                NodeError::NotLeader { .. } => ConsensusError::NotLeader,
                _ => {
                    error!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        error = %e,
                        "Failed to propose mutation to consensus"
                    );
                    ConsensusError::Internal(e.to_string())
                }
            })
        }
        .instrument(span)
        .await
    }

    async fn await_commit(&self, index: LogIndex) -> Result<(), ConsensusError> {
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "await_commit",
            index = %index
        );

        async {
            let mut progress_rx = self.state.subscribe();

            loop {
                // Check condition first (prevents missing updates before subscription)
                {
                    let progress = progress_rx.borrow();
                    match progress.role {
                        NodeRole::Leader => {
                            if progress.last_committed >= index {
                                return Ok(());
                            }
                        }
                        NodeRole::Poisoned => {
                            warn!(
                                target: ClinicalTarget::ClinicalFoundation.as_str(),
                                index = %index,
                                "Wait-for-Commit aborted: Node is POISONED"
                            );
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
        .instrument(span)
        .await
    }

    async fn await_apply(&self, index: LogIndex) -> Result<(), ConsensusError> {
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "await_apply",
            index = %index
        );

        async {
            let mut progress_rx = self.state.subscribe();

            loop {
                // Check condition first
                {
                    let progress = progress_rx.borrow();
                    if progress.role == NodeRole::Poisoned {
                        warn!(
                            target: ClinicalTarget::ClinicalFoundation.as_str(),
                            index = %index,
                            "Wait-for-Apply aborted: Node is POISONED"
                        );
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
        .instrument(span)
        .await
    }

    #[instrument(name = "authority_check", target = "raft::foundation", skip_all)]
    fn authority(&self) -> ConsensusAuthority {
        let progress = *self.state.subscribe().borrow();

        let (leader_hint, rejection_reason) =
            self.calculate_redirection(progress.role, progress.leader_hint);

        ConsensusAuthority {
            is_leader: progress.role == NodeRole::Leader,
            is_poisoned: progress.role == NodeRole::Poisoned,
            last_committed: progress.last_committed,
            leader_hint,
            rejection_reason,
        }
    }

    async fn verify_leadership(&self) -> Result<(), ConsensusError> {
        let trace_id = TraceId::generate();
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "verify_leadership",
            trace_id = %trace_id,
        );

        crate::orchestration::verify_leadership_quorum(
            &self.state,
            self.config.clone(),
            self.peer_manager.clone(),
            trace_id,
        )
        .instrument(span)
        .await
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
    use crate::engine::LogicalNode;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;
    use crate::tick::TickThresholds;

    #[derive(Debug, Default)]
    struct MockFsm;
    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        fn install_snapshot(
            &self,
            _index: LogIndex,
            _data: &[u8],
            _trace_id: TraceId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(1).unwrap(),
        ))
    }

    fn setup() -> (LocalRaftHandle<MockFsm>, Arc<ConsensusShell<MockFsm>>) {
        let id = mock_identity();
        let config = Arc::new(Config {
            cluster_id: id.cluster_id().clone(),
            node_id: id.node_id(),
            listen_addr: "127.0.0.1:50051".parse().unwrap(),
            data_dir: "data".into(),
            peers: std::collections::HashMap::new(),
            raft: crate::config::RaftConfig::default(),
            policy: crate::config::PolicyConfig::default(),
        });
        let fsm = Arc::new(MockFsm);
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = rand::SeedableRng::seed_from_u64(1);
        let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
        let state = Arc::new(ConsensusShell::new(node));
        let peer_manager =
            Arc::new(PeerManager::try_new(id, &std::collections::HashMap::new()).unwrap());
        (
            LocalRaftHandle::new(config, state.clone(), peer_manager),
            state,
        )
    }

    mod verify_leadership {
        use super::*;

        #[tokio::test]
        async fn guarantees_quorum_authority() {
            let (handle, state) = setup();

            // 1. Transition to leader
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![
                    NodeId::try_new(2).unwrap(),
                    NodeId::try_new(3).unwrap(),
                ]);
            }

            // 2. Start verification
            let handle_clone = Arc::new(handle);
            let task = tokio::spawn(async move { handle_clone.verify_leadership().await });

            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            // 3. Simulate network acks advancing the epoch
            {
                let mut guard = state.write().await;
                let leader = guard.as_leader_mut().unwrap();
                leader
                    .state_mut()
                    .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
            }

            let result = task.await.unwrap();
            assert!(result.is_ok());
        }
    }

    mod consensus_status {
        use super::*;

        #[tokio::test]
        async fn reports_correctly_for_follower_without_leader() {
            let (handle, _) = setup();
            let status = handle.authority();

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
            let status = handle.authority();

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
            assert_eq!(result.unwrap(), LogIndex::new(1));
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
                if guard.as_leader_mut().is_some() {
                    guard.advance_last_committed(index);
                }
            });

            let result = handle.await_commit(index).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn succeeds_immediately_if_already_committed() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![]);
            }

            let index = handle.propose(vec![1]).await.unwrap();

            // Advance state before calling await_commit
            {
                let mut guard = state.write().await;
                if guard.as_leader_mut().is_some() {
                    guard.advance_last_committed(index);
                }
            }

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

                // Advance last_committed (which no longer applies FSM inline).
                // Use advance_horizon_after_snapshot to simulate what the
                // background applier would do.
                if let Some(node) = guard.as_follower_mut() {
                    let mut entries = Vec::new();
                    for i in 1..=index.as_u64() {
                        entries.push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
                    }
                    node.log_store().append_entries(entries).unwrap();
                }

                guard.advance_last_committed(index);
                let _ = guard.advance_horizon_after_snapshot(index);
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
                if let Some(node) = guard.as_follower_mut() {
                    let mut entries = Vec::new();
                    for i in 1..=index.as_u64() {
                        entries.push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
                    }
                    node.log_store().append_entries(entries).unwrap();
                }
                guard.advance_last_committed(index);
                let _ = guard.advance_horizon_after_snapshot(index);
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
                guard.poison();
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
                guard.poison();
            }

            let status = handle.authority();
            assert!(!status.is_leader);
            assert_eq!(status.rejection_reason, "Node is in a poisoned state.");
        }

        #[tokio::test]
        async fn verify_leadership_returns_poisoned_error() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.poison();
            }

            let result = handle.verify_leadership().await;
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }

        #[tokio::test]
        async fn await_commit_fails_immediately_if_already_poisoned() {
            let (handle, state) = setup();
            {
                let mut guard = state.write().await;
                guard.poison();
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
                guard.poison();
                // MutationGuard drop triggers the watch channel update
            }

            let result = wait_task.await.unwrap();
            assert!(matches!(result, Err(ConsensusError::Poisoned)));
        }
    }
}
