use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
    in_flight_snapshots: StdMutex<HashSet<NodeId>>,
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
            in_flight_snapshots: StdMutex::new(HashSet::new()),
        }
    }

    /// Attempts to acquire a permit to initiate a snapshot replication for a
    /// peer.
    ///
    /// Returns a RAII guard if successful, or None if a snapshot is already in
    /// flight.
    pub fn try_acquire_snapshot_permit(
        self: &Arc<Self>,
        peer_id: NodeId,
    ) -> Option<SnapshotPermit<S>> {
        let mut in_flight = self.in_flight_snapshots.lock().unwrap();
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
        if let Ok(mut in_flight) = self.shell.in_flight_snapshots.lock() {
            in_flight.remove(&self.peer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::types::NodeId;

    use crate::test_utils::mock_shell;

    mod snapshot_permits {
        use super::*;

        #[test]
        fn only_one_permit_can_be_held_per_peer() {
            let shell = mock_shell();
            let peer_id = NodeId::try_new(2).unwrap();

            // 1. First permit acquisition succeeds
            let permit1 = shell.try_acquire_snapshot_permit(peer_id);
            assert!(permit1.is_some());

            // 2. Second permit acquisition for the SAME peer fails
            let permit2 = shell.try_acquire_snapshot_permit(peer_id);
            assert!(permit2.is_none());

            // 3. Acquisition for a DIFFERENT peer succeeds
            let peer_id_other = NodeId::try_new(3).unwrap();
            let permit3 = shell.try_acquire_snapshot_permit(peer_id_other);
            assert!(permit3.is_some());
        }

        #[test]
        fn permit_is_released_when_dropped() {
            let shell = mock_shell();
            let peer_id = NodeId::try_new(2).unwrap();

            {
                let _permit = shell.try_acquire_snapshot_permit(peer_id);
            }

            // Acquisition should succeed again (synchronously)
            let permit2 = shell.try_acquire_snapshot_permit(peer_id);
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
