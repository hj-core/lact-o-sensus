use std::fmt::Debug;

use async_trait::async_trait;
use common::types::LogIndex;
use common::types::errors::ConsensusError;

/// Atomic snapshot of the node's consensus authority and cluster horizon.
///
/// This structure provides a stable, lock-free view of the node's relationship
/// with the cluster. It is the primary mechanism used by the Gateway to
/// authorize mutations and linearizable queries.
#[derive(Debug, Clone, Default)]
pub struct ConsensusAuthority {
    /// True if this node is currently the authorized cluster leader.
    pub is_leader: bool,
    /// True if the node has encountered a fatal invariant and is halted.
    pub is_poisoned: bool,
    /// The current cluster-wide consistent horizon (commit_index).
    pub last_committed: LogIndex,
    /// The network address of the authorized leader, if known.
    pub leader_hint: String,
    /// Clinical explanation for why authority is absent or restricted.
    pub rejection_reason: String,
}

/// A generic interface for interacting with a local Raft consensus node.
///
/// This trait decouples application-specific gateway logic from the
/// underlying consensus engine (ADR 005/007).
#[async_trait]
pub trait ConsensusHandle: Send + Sync + Debug {
    /// Proposes an opaque payload to the consensus log.
    ///
    /// Returns the assigned LogIndex if successful.
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError>;

    /// Waits until the given index has been committed to a quorum.
    ///
    /// This method is intended to be **cancel-safe**. Callers should use
    /// executor-level timeouts (e.g., `tokio::time::timeout`) to enforce
    /// request SLAs.
    async fn await_commit(&self, index: LogIndex) -> Result<(), ConsensusError>;

    /// Waits until the given index has been applied to the local state machine.
    ///
    /// This is used to enforce Read-Your-Writes consistency (ADR 006).
    ///
    /// This method is intended to be **cancel-safe**. Callers should use
    /// executor-level timeouts (e.g., `tokio::time::timeout`) to enforce
    /// request SLAs.
    async fn await_apply(&self, index: LogIndex) -> Result<(), ConsensusError>;

    /// Returns a lock-free snapshot of the node's current consensus authority.
    ///
    /// This is the "Pre-flight Check" used to authorize external requests.
    fn authority(&self) -> ConsensusAuthority;

    /// Verifies that this node is still the current cluster leader.
    ///
    /// This method performs a network-bound quorum check (Batched Read Index)
    /// to ensure the node has not been deposed, providing the strict
    /// linearizability guarantee mandated by ADR 006.
    async fn verify_leadership(&self) -> Result<(), ConsensusError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    mod consensus_authority {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn returns_safe_defaults_when_initialized() {
                let status = ConsensusAuthority::default();
                assert!(!status.is_leader);
                assert!(!status.is_poisoned);
                assert_eq!(status.last_committed.as_u64(), 0);
                assert!(status.leader_hint.is_empty());
                assert!(status.rejection_reason.is_empty());
            }
        }
    }
}
