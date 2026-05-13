use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::sync::RwLockWriteGuard;
use tokio::sync::watch;

use crate::engine::ConsensusProgress;
use crate::engine::LogicalNode;

/// The Imperative Shell for consensus signaling.
///
/// This wrapper bundles the consensus state with its reactive signaling
/// channel, ensuring that all mutations are atomically broadcast once
/// logical consistency is reached.
#[derive(Debug)]
pub struct ConsensusShell {
    inner: Arc<RwLock<LogicalNode>>,
    progress_tx: watch::Sender<ConsensusProgress>,
}

impl ConsensusShell {
    /// Creates a new consensus state shell and initializes the signal channel.
    pub fn new(initial_state: LogicalNode) -> Self {
        let (progress_tx, _) = watch::channel(initial_state.consensus_progress());
        Self {
            inner: Arc::new(RwLock::new(initial_state)),
            progress_tx,
        }
    }

    /// Acquires a read lock on the consensus state.
    pub async fn read(&self) -> RwLockReadGuard<'_, LogicalNode> {
        self.inner.read().await
    }

    /// Acquires a mutation guard that atomically broadcasts any changes
    /// upon being dropped.
    ///
    /// LOCK-SIGNAL ATOMICITY (ADR 009): This is the ONLY approved way to
    /// mutate the consensus state. The returned guard ensures that any
    /// changes to the logical epoch or physical state are published to
    /// observers before the write lock is released.
    pub async fn write(&self) -> MutationGuard<'_> {
        let guard = self.inner.write().await;
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
pub struct MutationGuard<'a> {
    shell: &'a ConsensusShell,
    guard: RwLockWriteGuard<'a, LogicalNode>,
    before: ConsensusProgress,
}

impl<'a> Deref for MutationGuard<'a> {
    type Target = LogicalNode;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a> DerefMut for MutationGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<'a> Drop for MutationGuard<'a> {
    fn drop(&mut self) {
        // SAFETY: If the thread is already panicking, we MUST NOT trigger
        // a secondary panic in the destructor, as this leads to an abort.
        if std::thread::panicking() {
            return;
        }

        // Only attempt to broadcast if the node is NOT poisoned.
        // If it is poisoned, accessing it would trigger the Halt Mandate
        // panic, which we want to avoid in a destructor.
        if !self.guard.is_poisoned() {
            let after = self.guard.consensus_progress();
            if self.before != after {
                let _ = self.shell.progress_tx.send_replace(after);
            }
        }
    }
}
