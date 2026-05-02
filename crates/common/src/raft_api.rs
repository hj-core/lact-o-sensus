use async_trait::async_trait;
use tonic::Status;

use crate::types::ClientId;
use crate::types::LogIndex;
use crate::types::SequenceId;

/// Snapshot of the current consensus state relative to this node.
#[derive(Debug, Clone, Default)]
pub struct ConsensusStatus {
    /// True if this node currently believes itself to be the leader.
    pub is_leader: bool,
    /// The address of the current leader if known, or an empty string.
    pub leader_hint: String,
    /// A human-readable message explaining why mutations might be rejected.
    pub rejection_reason: String,
}

/// A generic interface for interacting with a local Raft consensus node.
///
/// This trait decouples application-specific gateway logic from the
/// underlying consensus engine (ADR 005/007).
#[async_trait]
pub trait RaftHandle: Send + Sync + std::fmt::Debug {
    /// Proposes an opaque payload to the consensus log.
    ///
    /// Returns the assigned LogIndex if successful.
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, Status>;

    /// Waits until the given index has been committed to a quorum.
    async fn await_commit(&self, index: LogIndex) -> Result<(), Status>;

    /// Returns a consistent snapshot of the node's current consensus status.
    /// This is preferred over individual checks to ensure atomicity in
    /// response generation.
    async fn consensus_status(&self) -> ConsensusStatus;

    /// Checks the session cache for deduplication (EOS).
    ///
    /// If the sequence ID has already been committed for this client,
    /// returns the LogIndex of the original mutation to allow the Gateway
    /// to return a cached success response without re-processing.
    async fn check_session(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
    ) -> Result<Option<LogIndex>, Status>;

    /// Verifies that this node is still the current cluster leader.
    ///
    /// For strict linearizability, this should perform a quorum check
    /// (e.g., a heartbeat round-trip) to ensure it hasn't been deposed.
    async fn verify_leadership(&self) -> Result<(), Status>;
}
