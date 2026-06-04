use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use common::raft_api::StateMachine;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::ConsensusError;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::sync::RwLockWriteGuard;
use tokio::sync::watch;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;

use crate::config::Config;
use crate::consensus::ReplicationRoundParams;
use crate::consensus::initiate_replication;
use crate::engine::ConsensusProgress;
use crate::engine::LogicalNode;
use crate::engine::NodeRole;
use crate::engine::SnapshotAction;
use crate::peer::PeerManager;
use crate::service::consensus::SnapshotParams;

/// The Imperative Shell for consensus signaling.
///
/// This wrapper bundles the consensus state with its reactive signaling
/// channel, ensuring that all mutations are atomically broadcast once
/// logical consistency is reached.
#[derive(Debug)]
pub struct ConsensusShell<S: StateMachine> {
    inner: Arc<RwLock<LogicalNode<S>>>,
    progress_tx: watch::Sender<ConsensusProgress>,
    apply_lock: Mutex<()>,
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
            apply_lock: Mutex::new(()),
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
        self.inner.read().await
    }

    /// Acquires a synchronous read lock on the consensus state.
    ///
    /// Should only be called within `spawn_blocking` contexts.
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, LogicalNode<S>> {
        self.inner.blocking_read()
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

    /// Acquires a synchronous mutation guard.
    ///
    /// Should only be called within `spawn_blocking` contexts.
    pub fn blocking_write(&self) -> MutationGuard<'_, S> {
        let mut guard = self.inner.blocking_write();
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

    /// Orchestrates the non-blocking application of committed entries to the
    /// State Machine.
    ///
    /// This method ensures that the primary consensus lock is NOT held across
    /// the FSM application boundary, preventing heartbeat starvation (ADR 009).
    #[instrument(
        name = "apply_committed_orchestration",
        target = "raft::replication",
        skip(self)
    )]
    pub async fn apply_committed(self: &Arc<Self>) {
        // [SERIALIZATION]: Ensure only one application loop runs at a time.
        // This prevents the race condition where concurrent tasks try to apply
        // the same log entry twice.
        let _permit = self.apply_lock.lock().await;

        let (fsm, log_store, mut applied, mut committed) = {
            let guard = self.read().await;
            (
                guard.fsm(),
                guard.log_store(),
                guard.last_applied(),
                guard.last_committed(),
            )
        };

        // Fatal Invariant Violation: Application must never exceed commitment.
        if applied > committed {
            let mut guard = self.write().await;
            guard.apply_fatal(NodeError::Protocol(format!(
                "Causal Divergence: applied ({}) > committed ({})",
                applied, committed
            )));
        }

        // Sequential application loop
        while applied < committed {
            let next_idx = match applied + 1u64 {
                Ok(idx) => idx,
                Err(e) => {
                    let mut guard = self.write().await;
                    guard.apply_fatal(NodeError::Arithmetic(e));
                }
            };

            // Phase 1: Read and Apply (Unlocked)
            // We read directly from the persistent log store. Since committed entries
            // are immutable, this is safe to do without the primary consensus lock.
            let entry = match log_store.read_entries(next_idx, next_idx) {
                Ok(entries) => entries.into_iter().next(),
                Err(e) => {
                    let mut guard = self.write().await;
                    guard.apply_fatal(NodeError::from(e));
                }
            };

            let apply_res = if let Some(entry) = entry {
                let fsm = fsm.clone();
                let data = entry.data.clone();
                match tokio::task::spawn_blocking(move || fsm.apply(next_idx, &data)).await {
                    Ok(result) => result,
                    Err(join_err) => {
                        let mut guard = self.write().await;
                        guard.apply_fatal(NodeError::Protocol(format!(
                            "spawn_blocking join error: {}",
                            join_err
                        )));
                    }
                }
            } else {
                let mut guard = self.write().await;
                guard.apply_fatal(NodeError::Protocol(format!(
                    "Committed entry {} missing from log storage",
                    next_idx
                )));
            };

            // Phase 2: Advance Horizon (Locked)
            {
                let mut guard = self.write().await;
                match apply_res {
                    Ok(_) => {
                        // Update volatile cache.
                        match guard.advance_horizon_after_snapshot(next_idx) {
                            Ok(_) => {
                                applied = next_idx;
                                committed = guard.last_committed();
                            }
                            Err(e) => {
                                error!(index = %next_idx, error = %e, "Failed to advance horizon");
                                guard.apply_fatal(e);
                            }
                        }
                    }
                    Err(e) => {
                        error!(index = %next_idx, error = %e, "FSM Apply failed");
                        guard.apply_fatal(NodeError::Protocol(format!("Apply failure: {}", e)));
                    }
                }
            }
        }
    }

    /// Orchestrates an InstallSnapshot request using a Non-Blocking Handoff.
    ///
    /// PHASE 1 (Locked): Validation and logical coordination.
    /// PHASE 2 (Unlocked): Offloaded background state machine restoration.
    /// PHASE 3 (Locked): Finalization and Freeze-Apply toggle.
    #[instrument(
        name = "shell_handle_install_snapshot",
        target = "raft::compaction",
        skip_all
    )]
    pub async fn handle_install_snapshot(
        self: &Arc<Self>,
        params: SnapshotParams,
    ) -> Result<Term, Status> {
        // Phase 1: Lock & Validate
        let (action, current_term, fsm) = {
            let mut guard = self.write().await;
            let res = guard.handle_install_snapshot(
                params.leader_id,
                params.term,
                params.last_included_index,
            );

            match res.action {
                SnapshotAction::Rejected => return Ok(res.term),
                SnapshotAction::Stale => return Ok(res.term),
                SnapshotAction::Accepted => {}
            }

            // Snapshot Accepted: Set Freeze-Apply state
            guard.set_snapshotting(true);
            (res.action, res.term, guard.fsm())
        };

        // Phase 2: Background restoration (Unlocked)
        if action == SnapshotAction::Accepted {
            let shell_clone = self.clone();
            let index = params.last_included_index;
            let term = params.last_included_term;
            let data = params.data;
            let trace_id = params.trace_id;

            // ADR 011: Use spawn_blocking for heavy FSM I/O to preserve the tick loop
            tokio::task::spawn_blocking(move || {
                let span = info_span!(
                    target: ClinicalTarget::RaftCompaction.as_str(),
                    "background_snapshot_install",
                    index = %index,
                    trace_id = %trace_id
                );
                let _enter = span.enter();

                info!("Starting background snapshot installation...");

                let res = fsm.install_snapshot(index, &data, trace_id);

                // Phase 3: Lock & Finalize
                let mut guard = shell_clone.blocking_write();
                guard.set_snapshotting(false);

                match res {
                    Ok(_) => {
                        info!(index = %index, "Background snapshot installation complete.");
                        guard.save_snapshot_metadata(index, term);

                        // ADR 011: Advance BOTH commit_index and volatile last_applied
                        // to ensure the next application loop starts after the snapshot.
                        if let Err(e) = guard.advance_horizon_after_snapshot(index) {
                            error!(index = %index, error = %e, "FATAL: Failed to advance logical horizon after snapshot.");
                            guard.apply_fatal(e);
                        }
                    }
                    Err(e) => {
                        error!(index = %index, error = %e, "FATAL: Snapshot installation failed.");
                        guard.apply_fatal(NodeError::Protocol(format!(
                            "Background snapshot restoration failure: {}",
                            e
                        )));
                    }
                }
            });
        }

        Ok(current_term)
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

    mod handle_install_snapshot {
        use std::sync::Barrier;

        use common::raft_api::StateMachine;

        use super::*;

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
                shell.handle_install_snapshot(params).await.unwrap();

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
}
