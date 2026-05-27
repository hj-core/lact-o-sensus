use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::errors::ConsensusError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::sync::RwLockWriteGuard;
use tokio::sync::watch;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info_span;
use tracing::instrument;

use crate::config::Config;
use crate::consensus::ReplicationRoundParams;
use crate::consensus::initiate_replication;
use crate::engine::ConsensusProgress;
use crate::engine::LogicalNode;
use crate::engine::NodeRole;
use crate::peer::PeerManager;

/// The Imperative Shell for consensus signaling.
///
/// This wrapper bundles the consensus state with its reactive signaling
/// channel, ensuring that all mutations are atomically broadcast once
/// logical consistency is reached.
#[derive(Debug)]
pub struct ConsensusShell<S: StateMachine> {
    inner: Arc<RwLock<LogicalNode<S>>>,
    progress_tx: watch::Sender<ConsensusProgress>,
}

impl<S: StateMachine> ConsensusShell<S> {
    /// Creates a new consensus state shell and initializes the signal channel.
    pub fn new(mut initial_state: LogicalNode<S>) -> Self {
        let progress = initial_state.consensus_progress();

        let (progress_tx, _) = watch::channel(progress);
        Self {
            inner: Arc::new(RwLock::new(initial_state)),
            progress_tx,
        }
    }

    /// Acquires a read lock on the consensus state.
    pub async fn read(&self) -> RwLockReadGuard<'_, LogicalNode<S>> {
        self.inner.read().await
    }

    /// Acquires a mutation guard that atomically broadcasts any changes
    /// upon being dropped.
    ///
    /// LOCK-SIGNAL ATOMICITY (ADR 009): This is the ONLY approved way to
    /// mutate the consensus state. The returned guard ensures that any
    /// changes to the logical epoch or physical state are published to
    /// observers before the write lock is released.
    #[instrument(name = "acquire_mutation_lock", target = "raft::foundation", skip_all)]
    pub async fn write(&self) -> MutationGuard<'_, S> {
        let mut guard = self.inner.write().await;
        let before = guard.consensus_progress();
        MutationGuard {
            shell: self,
            guard,
            before,
        }
    }

    /// Provides a new subscription to the consensus progress stream.
    pub fn subscribe(&self) -> watch::Receiver<ConsensusProgress> {
        self.progress_tx.subscribe()
    }

    /// Performs a network-bound quorum check to verify leadership (§8).
    ///
    /// Triggers an immediate heartbeat broadcast and awaits confirmation from
    /// a majority of peers. Guarantees strict linearizability (ADR 006).
    pub async fn verify_leadership_quorum(
        self: &Arc<Self>,
        config: Arc<Config>,
        peer_manager: Arc<PeerManager>,
        trace_id: TraceId,
    ) -> Result<(), ConsensusError> {
        // 1. Prepare the probe and record target epoch.
        // We use explicit health and role checks here to avoid the delegate_to_inner!
        // macro, which panics on poisoned nodes. This allows the API to return a
        // structured error instead of a process-wide panic (ADR 009).
        let (target_epoch, already_in_flight, term, node_id, last_committed) = {
            let mut guard = self.write().await;

            if guard.is_poisoned() {
                return Err(ConsensusError::Poisoned);
            }

            let node_id = guard.node_id();
            let last_committed = guard.last_committed();

            if let Some(leader) = guard.as_leader_mut() {
                let term = leader.current_term().map_err(|_| {
                    ConsensusError::Internal("Leadership verification failed".to_string())
                })?;
                let self_id = leader.node_id();
                let current = leader.state().current_read_epoch();
                let target = leader.state_mut().prepare_read_probe(self_id);
                (target, target == current, term, node_id, last_committed)
            } else {
                return Err(ConsensusError::NotLeader);
            }
        };

        // 2. Establish child span for mechanism visibility
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "quorum_probe",
            node_id = %node_id,
            trace_id = %trace_id,
            target_epoch = %target_epoch,
            term = %term
        );

        async {
            let mut progress_rx = self.subscribe();
            let timeout_dur = config.raft.rpc_timeout();

            // 3. Trigger immediate replication (heartbeat broadcast) if a new round is
            //    needed.
            if !already_in_flight {
                initiate_replication(
                    config,
                    self.clone(),
                    peer_manager,
                    ReplicationRoundParams {
                        term,
                        node_id,
                        last_committed,
                        trace_id,
                    },
                    span.clone(),
                );
            }

            // 4. Await quorum confirmation, demotion, or timeout.
            loop {
                // Check if already reached
                {
                    let progress = progress_rx.borrow();
                    if progress.role != NodeRole::Leader || progress.term != term {
                        return Err(ConsensusError::NotLeader);
                    }
                    if progress.confirmed_read_epoch >= target_epoch {
                        return Ok(());
                    }
                }

                tokio::select! {
                    Ok(_) = progress_rx.changed() => {
                        // Loop will check condition
                    }
                    _ = tokio::time::sleep(timeout_dur) => {
                        return Err(ConsensusError::Timeout);
                    }
                }
            }
        }
        .instrument(span.clone())
        .await
    }
}

/// A RAII guard that enforces Lock-Signal Atomicity.
///
/// When this guard is dropped, it compares the consensus state before and
/// after the mutation. If the state (or the logical epoch) has changed,
/// it broadcasts the new progress snapshot to all observers.
pub struct MutationGuard<'a, S: StateMachine> {
    shell: &'a ConsensusShell<S>,
    guard: RwLockWriteGuard<'a, LogicalNode<S>>,
    before: ConsensusProgress,
}

impl<'a, S: StateMachine> Deref for MutationGuard<'a, S> {
    type Target = LogicalNode<S>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, S: StateMachine> DerefMut for MutationGuard<'a, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<'a, S: StateMachine> Drop for MutationGuard<'a, S> {
    fn drop(&mut self) {
        // We determine the 'after' state. If the thread is already panicking,
        // we MUST transition the node to Poisoned and broadcast a terminal
        // signal to halt other tasks (ADR 009).
        let after = if std::thread::panicking() {
            self.guard.poison();
            error!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                "HALT MANDATE: Thread panicked while holding mutation lock. Poisoning node."
            );
            ConsensusProgress {
                term: self.before.term,
                role: NodeRole::Poisoned,
                last_log_index: self.before.last_log_index,
                last_committed: self.before.last_committed,
                last_applied: self.before.last_applied,
                leader_hint: None,
                confirmed_read_epoch: self.before.confirmed_read_epoch,
            }
        } else if self.guard.is_poisoned() {
            ConsensusProgress {
                term: self.before.term,
                role: NodeRole::Poisoned,
                last_log_index: self.before.last_log_index,
                last_committed: self.before.last_committed,
                last_applied: self.before.last_applied,
                leader_hint: None,
                confirmed_read_epoch: self.before.confirmed_read_epoch,
            }
        } else {
            // In the normal case, we extract the progress from the healthy node.
            // We use the try_ version here to avoid a panic-in-drop if the logic
            // itself has a bug.
            self.guard.try_consensus_progress().unwrap_or(self.before)
        };

        // Broadcast if anything has changed (Term, Role, Index, or Poison).
        if self.before != after {
            debug!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                old_role = ?self.before.role,
                new_role = ?after.role,
                "Atomic State Signal Broadcast"
            );
            let _ = self.shell.progress_tx.send_replace(after);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use common::types::ClusterId;
    use common::types::LogIndex;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use common::types::Term;
    use common::types::errors::FsmError;
    use rand::SeedableRng;

    use super::*;
    use crate::peer::PeerManager;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;
    use crate::tick::TickThresholds;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait::async_trait]
    impl common::raft_api::StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        async fn install_snapshot(
            &self,
            _index: LogIndex,
            _data: &[u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn test_config() -> Config {
        Config {
            cluster_id: ClusterId::try_new("test").unwrap(),
            node_id: NodeId::try_new(1).unwrap(),
            listen_addr: "127.0.0.1:8080".parse().unwrap(),
            data_dir: "data".into(),
            peers: HashMap::new(),
            raft: crate::config::RaftConfig::default(),
            policy: crate::config::PolicyConfig::default(),
        }
    }

    fn mock_shell() -> Arc<ConsensusShell<MockFsm>> {
        let id = Arc::new(NodeIdentity::new(
            ClusterId::try_new("test").unwrap(),
            NodeId::try_new(1).unwrap(),
        ));
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = rand::rngs::StdRng::seed_from_u64(1);
        let node = LogicalNode::try_new(id, Arc::new(MockFsm), storage, thresholds, rng).unwrap();
        Arc::new(ConsensusShell::new(node))
    }

    mod verify_leadership_quorum {
        use super::*;

        mod with_stable_leader {
            use super::*;

            #[tokio::test]
            async fn completes_successfully_and_batches_concurrent_reads_in_single_round() {
                let shell = mock_shell();
                let config = Arc::new(test_config());
                let peer_manager = Arc::new(
                    PeerManager::try_new(shell.read().await.identity(), &HashMap::new()).unwrap(),
                );

                // 1. Transition to leader
                {
                    let mut guard = shell.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![
                        NodeId::try_new(2).unwrap(),
                        NodeId::try_new(3).unwrap(),
                    ]);
                }

                // 2. Start two concurrent reads
                let shell1 = shell.clone();
                let config1 = config.clone();
                let pm1 = peer_manager.clone();
                let read1 = tokio::spawn(async move {
                    shell1
                        .verify_leadership_quorum(config1, pm1, TraceId::generate())
                        .await
                });

                let shell2 = shell.clone();
                let config2 = config.clone();
                let pm2 = peer_manager.clone();
                let read2 = tokio::spawn(async move {
                    shell2
                        .verify_leadership_quorum(config2, pm2, TraceId::generate())
                        .await
                });

                // Give them time to register and start waiting
                tokio::time::sleep(Duration::from_millis(20)).await;

                // 3. Verify they are both waiting for the SAME epoch (batching)
                {
                    let guard = shell.read().await;
                    let leader = guard.as_leader().unwrap();
                    assert_eq!(leader.state().current_read_epoch(), 1);
                }

                // 4. Simulate quorum acknowledgment
                {
                    let mut guard = shell.write().await;
                    let leader = guard.as_leader_mut().unwrap();
                    leader
                        .state_mut()
                        .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
                    // Drop triggers signal
                }

                // 5. Verify both reads complete successfully
                let res1 = read1.await.unwrap();
                let res2 = read2.await.unwrap();

                assert!(res1.is_ok());
                assert!(res2.is_ok());
            }
        }

        mod with_unstable_leader {
            use super::*;

            #[tokio::test]
            async fn returns_error_on_demotion() {
                let shell = mock_shell();
                let config = Arc::new(test_config());
                let peer_manager = Arc::new(
                    PeerManager::try_new(shell.read().await.identity(), &HashMap::new()).unwrap(),
                );

                // 1. Transition to leader
                {
                    let mut guard = shell.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![NodeId::try_new(2).unwrap()]);
                }

                let shell_clone = shell.clone();
                let config_clone = config.clone();
                let pm_clone = peer_manager.clone();
                let task = tokio::spawn(async move {
                    shell_clone
                        .verify_leadership_quorum(config_clone, pm_clone, TraceId::generate())
                        .await
                });

                tokio::time::sleep(Duration::from_millis(20)).await;

                // 2. Simulate demotion
                {
                    let mut guard = shell.write().await;
                    guard.into_follower(Term::new(10), None);
                }

                let result = task.await.unwrap();
                assert!(matches!(result, Err(ConsensusError::NotLeader)));
            }
        }
    }
}
