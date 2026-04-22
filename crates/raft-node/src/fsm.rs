use async_trait::async_trait;
use common::types::LogIndex;
use tonic::Status;

/// Boundary trait between the generic Raft consensus engine and the
/// application logic.
///
/// Implementations are responsible for deserializing the opaque bytes and
/// applying the mutation to their internal state.
#[async_trait]
pub trait StateMachine: Send + Sync + std::fmt::Debug {
    /// Applies a committed log entry to the application state.
    ///
    /// This method is called sequentially by the Raft engine as the
    /// commit_index advances.
    async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Status>;
}
