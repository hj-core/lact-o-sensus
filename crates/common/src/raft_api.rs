//! System-wide clinical contracts and boundary traits for Lact-O-Sensus.
//!
//! This module defines the core interfaces between the Raft consensus engine
//! and the application-specific logic. It facilitates the "Decoupled Oracle"
//! and "Clean Architecture" models by providing opaque handles for consensus
//! and structured traits for state machine interactions.

use std::fmt::Debug;
use std::fmt::Display;

use crate::types::LogIndex;
use crate::types::errors::NodeError;
use crate::types::trace::TraceId;

/// Boundary trait between the generic Raft consensus engine and the
/// application logic.
///
/// Implementations are responsible for deserializing the opaque bytes and
/// applying the mutation to their internal state.
pub trait StateMachine: Send + Sync + Debug + 'static {
    /// The clinical error type returned by the state machine.
    ///
    /// Must be convertible to NodeError to satisfy the Halt Mandate (ADR 009).
    type Error: Into<NodeError> + Send + Sync + Debug + Display;

    /// Returns the last log index applied to this state machine.
    ///
    /// Used by the Raft engine during startup to align volatile pointers
    /// with persistent application state.
    fn last_applied_index(&self) -> Result<LogIndex, Self::Error>;

    /// Applies a committed log entry to the application state.
    ///
    /// This method is called sequentially by the Raft engine as the
    /// commit_index advances.
    fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Self::Error>;

    /// Captures a consistent, serializable snapshot of the entire state
    /// machine.
    ///
    /// The returned byte vector MUST contain all state required to reconstruct
    /// the application to this exact point-in-time, including inventory and
    /// session metadata (ADR 011).
    fn snapshot(&self) -> Result<Vec<u8>, Self::Error>;

    /// Restores the entire application state from a provided snapshot.
    ///
    /// This is an atomic operation; the implementation MUST clear all existing
    /// state before restoring from the provided bytes. The given
    /// `last_included_index` represents the logical horizon of the snapshot
    /// and MUST be persisted as the new last_applied index (ADR 011).
    ///
    /// If restoration fails, the node MUST transition to a Poisoned state (ADR
    /// 009).
    fn install_snapshot(
        &self,
        last_included_index: LogIndex,
        data: &[u8],
        trace_id: TraceId,
    ) -> Result<(), Self::Error>;
}
