use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use common::raft_api::StateMachine;
use common::types::NodeId;
use common::types::trace::ClinicalTarget;
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use parking_lot::RwLockWriteGuard;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;

use crate::engine::ConsensusProgress;
use crate::engine::LogicalNode;
use crate::engine::NodeRole;

/// The Imperative Shell for consensus signaling.
///
/// This wrapper bundles the consensus state with its reactive signaling
/// channel, ensuring that all mutations are atomically broadcast once
/// logical consistency is reached.
#[derive(Debug)]
pub struct ConsensusShell<S: StateMachine> {
    inner: Arc<RwLock<LogicalNode<S>>>,
    progress_tx: watch::Sender<ConsensusProgress>,
    /// Guards all FSM I/O (`apply`, `snapshot`, `install_snapshot`) to
    /// prevent concurrent state machine access from overlapping tasks
    /// (ADR 009, ADR 011).
    pub(crate) fsm_lock: Mutex<()>,
    /// Reference-counted freeze depth (ADR 011). The FSM is considered frozen
    /// for application as long as this counter is greater than zero. Managed
    /// via `freeze()` / `thaw()`; read via `is_frozen()`.
    fsm_freeze_depth: AtomicU32,
    /// Track peers with an active snapshot replication in flight.
    in_flight_snapshots: Mutex<HashSet<NodeId>>,
}

/// Returned by `freeze()` / `thaw()` when the freeze-depth invariant is
/// violated (overflow on freeze, underflow on thaw). This is a local
/// caller-contract error — the holder of the `MutationGuard` should pass it
/// to `apply_fatal` to poison the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeInvariantError(pub &'static str);

impl<S: StateMachine> ConsensusShell<S> {
    /// Creates a new consensus state shell and initializes the signal channel.
    pub fn new(mut initial_state: LogicalNode<S>) -> Self {
        let progress = initial_state.consensus_progress();

        let (progress_tx, _) = watch::channel(progress);
        Self {
            inner: Arc::new(RwLock::new(initial_state)),
            progress_tx,
            fsm_lock: Mutex::new(()),
            fsm_freeze_depth: AtomicU32::new(0),
            in_flight_snapshots: Mutex::new(HashSet::new()),
        }
    }

    /// Attempts to acquire a permit to initiate a snapshot replication for a
    /// peer.
    ///
    /// Returns a RAII guard if successful, or None if a snapshot is already in
    /// flight.
    pub async fn try_acquire_snapshot_permit(
        self: &Arc<Self>,
        peer_id: NodeId,
    ) -> Option<SnapshotPermit<S>> {
        let mut in_flight = self.in_flight_snapshots.lock().await;
        if in_flight.insert(peer_id) {
            Some(SnapshotPermit {
                shell: self.clone(),
                peer_id,
            })
        } else {
            None
        }
    }

    /// Acquires a read lock on the consensus state.
    pub async fn read(&self) -> RwLockReadGuard<'_, LogicalNode<S>> {
        loop {
            if let Some(guard) = self.inner.try_read() {
                return guard;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Acquires a synchronous read lock on the consensus state.
    ///
    /// Should only be called within `spawn_blocking` contexts.
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, LogicalNode<S>> {
        self.inner.read()
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
        loop {
            if let Some(mut guard) = self.inner.try_write() {
                let before = guard.consensus_progress();
                return MutationGuard {
                    shell: self,
                    guard,
                    before,
                };
            }
            tokio::task::yield_now().await;
        }
    }

    /// Acquires a synchronous mutation guard.
    ///
    /// Should only be called within `spawn_blocking` contexts.
    pub fn blocking_write(&self) -> MutationGuard<'_, S> {
        let mut guard = self.inner.write();
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

    /// Increments the FSM freeze depth.
    ///
    /// Returns `Err(FreezeInvariantError)` on overflow (more than u32::MAX
    /// concurrent freezes). The caller (who holds the `MutationGuard`) should
    /// pass the error to `apply_fatal` to poison the node.
    pub fn freeze(&self) -> Result<(), FreezeInvariantError> {
        let prev = self.fsm_freeze_depth.fetch_add(1, Ordering::AcqRel);
        if prev == u32::MAX {
            self.fsm_freeze_depth.fetch_sub(1, Ordering::Release);
            error!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                depth = prev + 1,
                "FATAL: fsm_freeze_depth overflow"
            );
            return Err(FreezeInvariantError(
                "fsm_freeze_depth overflow (u32::MAX concurrent freezes)",
            ));
        }
        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            depth = prev + 1,
            "FSM frozen."
        );
        Ok(())
    }

    /// Decrements the FSM freeze depth.
    ///
    /// Returns `Err(FreezeInvariantError)` on underflow (thaw without matching
    /// freeze). The caller should pass the error to `apply_fatal` to poison
    /// the node.
    pub fn thaw(&self) -> Result<(), FreezeInvariantError> {
        let prev = self.fsm_freeze_depth.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            self.fsm_freeze_depth.fetch_add(1, Ordering::Release);
            error!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                depth = prev,
                "FATAL: fsm_freeze_depth underflow (thaw without matching freeze)"
            );
            return Err(FreezeInvariantError(
                "fsm_freeze_depth underflow (thaw without matching freeze)",
            ));
        }
        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            depth = prev - 1,
            "FSM thawed."
        );
        Ok(())
    }

    /// Returns true when the FSM is currently frozen (freeze depth > 0).
    pub fn is_frozen(&self) -> bool {
        self.fsm_freeze_depth.load(Ordering::Acquire) > 0
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

/// A RAII guard that tracks an in-flight snapshot replication.
///
/// When dropped, it removes the peer from the in-flight set, allowing
/// subsequent snapshot attempts.
#[derive(Debug)]
pub struct SnapshotPermit<S: StateMachine> {
    shell: Arc<ConsensusShell<S>>,
    peer_id: NodeId,
}

impl<S: StateMachine> Drop for SnapshotPermit<S> {
    fn drop(&mut self) {
        // We use a background task here because Drop cannot be async.
        // ADR 011: This is a lightweight set operation and is safe for the executor.
        let shell = self.shell.clone();
        let peer_id = self.peer_id;
        tokio::spawn(async move {
            let mut in_flight = shell.in_flight_snapshots.lock().await;
            in_flight.remove(&peer_id);
        });
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
    use common::types::errors::FsmError;
    use common::types::trace::TraceId;
    use rand::SeedableRng;

    use super::*;
    use crate::config::Config;
    use crate::peer::PeerManager;
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
        use common::types::Term;
        use common::types::errors::ConsensusError;
        use common::types::trace::TraceId;

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
                    crate::orchestration::verify_leadership_quorum(
                        &shell1,
                        config1,
                        pm1,
                        TraceId::generate(),
                    )
                    .await
                });

                let shell2 = shell.clone();
                let config2 = config.clone();
                let pm2 = peer_manager.clone();
                let read2 = tokio::spawn(async move {
                    crate::orchestration::verify_leadership_quorum(
                        &shell2,
                        config2,
                        pm2,
                        TraceId::generate(),
                    )
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
                    crate::orchestration::verify_leadership_quorum(
                        &shell_clone,
                        config_clone,
                        pm_clone,
                        TraceId::generate(),
                    )
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

    mod handle_install_snapshot {
        use std::sync::Barrier;

        use common::raft_api::StateMachine;
        use common::types::Term;
        use common::types::trace::TraceId;

        use super::*;
        use crate::service::consensus::SnapshotParams;

        #[derive(Debug)]
        struct MockDelayedFsm {
            barrier: Arc<Barrier>,
        }

        impl StateMachine for MockDelayedFsm {
            type Error = FsmError;

            fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                Ok(LogIndex::ZERO)
            }

            fn apply(&self, _idx: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                Ok(())
            }

            fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                Ok(vec![])
            }

            fn install_snapshot(
                &self,
                _idx: LogIndex,
                _data: &[u8],
                _trace_id: TraceId,
            ) -> Result<(), Self::Error> {
                // BLOCK here until the test signals we can continue
                // ADR 011: This is safe to do in a synchronous block because
                // the shell calls this method within tokio::task::spawn_blocking.
                self.barrier.wait();
                Ok(())
            }
        }

        mod non_blocking_behavior {
            use super::*;
            use crate::engine::TickAction;

            #[tokio::test]
            async fn tick_loop_continues_during_heavy_restoration() {
                let barrier = Arc::new(Barrier::new(2));
                let fsm = Arc::new(MockDelayedFsm {
                    barrier: barrier.clone(),
                });

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
                let node = LogicalNode::try_new(id, fsm, storage, thresholds, rng).unwrap();
                let shell = Arc::new(ConsensusShell::new(node));

                let params = SnapshotParams {
                    leader_id: NodeId::try_new(2).unwrap(),
                    term: Term::new(1),
                    last_included_index: LogIndex::new(100),
                    last_included_term: Term::new(1),
                    data: vec![1, 2, 3],
                    trace_id: TraceId::generate(),
                };

                // 1. Trigger heavy restoration (which will block on the barrier in a background
                //    task)
                crate::orchestration::handle_install_snapshot(&shell, params)
                    .await
                    .unwrap();

                // 2. Verify we can immediately acquire the lock and perform a tick.
                // If it were blocking, we'd be stuck here because the background task
                // would still be holding some resource or the shell would be in a blocked
                // state.
                let mut guard = shell.write().await;
                let action = guard.tick();

                // 3. Confirm the tick occurred (proving the shell is alive and responsive)
                assert!(matches!(
                    action,
                    TickAction::None | TickAction::StartElection
                ));

                // 4. Release the lock to allow the background task to complete and acquire it
                drop(guard);

                // 5. Release the background task so it can finish the InstallSnapshot phase
                barrier.wait();

                // Give the background task a tiny moment to acquire the lock and update the
                // state
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // 6. Verify that the logical horizon was advanced correctly
                let guard = shell.read().await;
                assert_eq!(guard.last_committed(), LogIndex::new(100));
                assert_eq!(guard.last_applied(), LogIndex::new(100));
            }
        }
    }

    mod snapshot_permits {
        use super::*;

        #[tokio::test]
        async fn only_one_permit_can_be_held_per_peer() {
            let shell = mock_shell();
            let peer_id = NodeId::try_new(2).unwrap();

            // 1. First permit acquisition succeeds
            let permit1 = shell.try_acquire_snapshot_permit(peer_id).await;
            assert!(permit1.is_some());

            // 2. Second permit acquisition for the SAME peer fails
            let permit2 = shell.try_acquire_snapshot_permit(peer_id).await;
            assert!(permit2.is_none());

            // 3. Acquisition for a DIFFERENT peer succeeds
            let peer_id_other = NodeId::try_new(3).unwrap();
            let permit3 = shell.try_acquire_snapshot_permit(peer_id_other).await;
            assert!(permit3.is_some());
        }

        #[tokio::test]
        async fn permit_is_released_when_dropped() {
            let shell = mock_shell();
            let peer_id = NodeId::try_new(2).unwrap();

            {
                let _permit = shell.try_acquire_snapshot_permit(peer_id).await;
            }

            // Drop is async (spawned), so give it a tiny moment
            tokio::time::sleep(Duration::from_millis(5)).await;

            // Acquisition should succeed again
            let permit2 = shell.try_acquire_snapshot_permit(peer_id).await;
            assert!(permit2.is_some());
        }
    }

    mod fsm_freeze_depth {
        use super::*;

        #[test]
        fn initially_not_frozen() {
            let shell = mock_shell();

            assert!(!shell.is_frozen());
        }

        #[test]
        fn freeze_makes_frozen() {
            let shell = mock_shell();

            shell.freeze().unwrap();

            assert!(shell.is_frozen());
        }

        #[test]
        fn freeze_then_thaw_returns_to_unfrozen() {
            let shell = mock_shell();

            shell.freeze().unwrap();
            shell.thaw().unwrap();

            assert!(!shell.is_frozen());
        }

        #[test]
        fn freeze_depth_tracks_nesting() {
            let shell = mock_shell();

            shell.freeze().unwrap();
            shell.freeze().unwrap();
            assert!(shell.is_frozen());

            shell.thaw().unwrap();
            assert!(shell.is_frozen());

            shell.thaw().unwrap();
            assert!(!shell.is_frozen());
        }

        /// Overflow path (`freeze` called u32::MAX times) is not directly
        /// testable without internal access to the `AtomicU32` field.
        #[test]
        fn thaw_underflow_returns_error() {
            let shell = mock_shell();

            let err = shell.thaw().unwrap_err();

            assert!(err.0.contains("underflow"));
        }

        #[test]
        fn multiple_freezes_keep_depth_nonzero() {
            let shell = mock_shell();

            shell.freeze().unwrap();
            shell.freeze().unwrap();
            shell.freeze().unwrap();

            assert!(shell.is_frozen());

            shell.thaw().unwrap();
            shell.thaw().unwrap();
            shell.thaw().unwrap();

            assert!(!shell.is_frozen());
        }
    }

    mod rwlock_contention {
        use super::*;

        mod under_rapid_short_writes_followed_by_long_write {
            use super::*;

            #[tokio::test]
            async fn subsequent_write_lock_acquires_within_timeout() {
                let shell = mock_shell();

                // Phase 1: 200 rapid short write-lock acquisitions (like a tick loop)
                for _ in 0..200 {
                    let mut guard = shell.write().await;
                    guard.tick();
                    drop(guard);
                }

                // Phase 2: One longer write-lock acquisition (simulating sled flush)
                {
                    let guard = shell.write().await;
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(2) {
                        std::hint::spin_loop();
                    }
                    drop(guard);
                }

                // Phase 3: Verify the next writer can acquire within a short timeout.
                let result = tokio::time::timeout(Duration::from_secs(5), async {
                    let mut guard = shell.write().await;
                    guard.tick();
                })
                .await;

                assert!(
                    result.is_ok(),
                    "Write lock permanently stalled after contention pattern"
                );
            }

            #[tokio::test]
            async fn concurrent_readers_are_not_starved() {
                let shell = mock_shell();

                // Phase 1: Same contention pattern
                for _ in 0..200 {
                    let mut guard = shell.write().await;
                    guard.tick();
                    drop(guard);
                }

                // Phase 2: Hold write lock briefly
                {
                    let guard = shell.write().await;
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(2) {
                        std::hint::spin_loop();
                    }
                    drop(guard);
                }

                // Phase 3: Spawn concurrent readers, all must complete within timeout
                let mut handles = Vec::new();
                for _ in 0..10 {
                    let shell = shell.clone();
                    handles.push(tokio::spawn(async move {
                        let guard = shell.read().await;
                        assert!(guard.try_current_term().is_ok());
                    }));
                }
                for handle in handles {
                    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
                    assert!(result.is_ok(), "Concurrent reader was starved");
                    assert!(result.unwrap().is_ok(), "Concurrent reader task failed");
                }
            }
        }
    }
}
