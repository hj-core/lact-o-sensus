use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

use common::raft_api::StateMachine;
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::sync::RwLockWriteGuard;
use tokio::sync::watch;

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
            ConsensusProgress {
                term: self.before.term,
                role: NodeRole::Poisoned,
                last_log_index: self.before.last_log_index,
                last_committed: self.before.last_committed,
                last_applied: self.before.last_applied,
                leader_hint: None,
            }
        } else if self.guard.is_poisoned() {
            ConsensusProgress {
                term: self.before.term,
                role: NodeRole::Poisoned,
                last_log_index: self.before.last_log_index,
                last_committed: self.before.last_committed,
                last_applied: self.before.last_applied,
                leader_hint: None,
            }
        } else {
            // In the normal case, we extract the progress from the healthy node.
            // We use the try_ version here to avoid a panic-in-drop if the logic
            // itself has a bug.
            self.guard.try_consensus_progress().unwrap_or(self.before)
        };

        // Broadcast if anything has changed (Term, Role, Index, or Poison).
        if self.before != after {
            let _ = self.shell.progress_tx.send_replace(after);
        }
    }
}
