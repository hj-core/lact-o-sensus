use thiserror::Error;

use crate::types::LogIndex;

// =============================================================================
// 1. Clinical Orchestration (NodeError)
// =============================================================================

/// Categorical system errors that trigger the Halt Mandate (ADR 009).
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("Physical Integrity Violation: {0}")]
    Physical(String),

    #[error("Logical Integrity Violation: {0}")]
    Logical(String),

    #[error("Semantic Integrity Violation: {0}")]
    Semantic(String),

    #[error("Identity Integrity Violation: {0}")]
    Identity(String),
}

impl From<LogStorageError> for NodeError {
    fn from(err: LogStorageError) -> Self {
        match err {
            LogStorageError::Persistence(msg) => NodeError::Physical(msg),
            LogStorageError::Serialization(msg) => NodeError::Semantic(msg),
            LogStorageError::Deserialization(msg) => NodeError::Semantic(msg),
            LogStorageError::Invariant(msg) => NodeError::Logical(msg),
        }
    }
}

impl From<FsmError> for NodeError {
    fn from(err: FsmError) -> Self {
        match err {
            FsmError::Persistence(msg) => NodeError::Physical(msg),
            FsmError::Serialization(msg) => NodeError::Semantic(msg),
            FsmError::Deserialization(msg) => NodeError::Semantic(msg),
            FsmError::Invariant(msg) => NodeError::Logical(msg),
        }
    }
}

// =============================================================================
// 2. Physical Foundation (LogStorageError)
// =============================================================================

#[derive(Debug, Error)]
pub enum LogStorageError {
    #[error("Persistence failure: {0}")]
    Persistence(String),

    #[error("Serialization failure: {0}")]
    Serialization(String),

    #[error("Deserialization failure: {0}")]
    Deserialization(String),

    #[error("Foundation invariant violation: {0}")]
    Invariant(String),
}

impl LogStorageError {
    pub fn persistence(msg: impl Into<String>) -> Self {
        Self::Persistence(msg.into())
    }

    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::Serialization(msg.into())
    }

    pub fn deserialization(msg: impl Into<String>) -> Self {
        Self::Deserialization(msg.into())
    }

    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }
}

// =============================================================================
// 3. Logical Foundation (FsmError)
// =============================================================================

#[derive(Debug, Error)]
pub enum FsmError {
    #[error("Persistence failure: {0}")]
    Persistence(String),

    #[error("Serialization failure: {0}")]
    Serialization(String),

    #[error("Deserialization failure: {0}")]
    Deserialization(String),

    #[error("Foundation invariant violation: {0}")]
    Invariant(String),
}

impl FsmError {
    pub fn persistence(msg: impl Into<String>) -> Self {
        Self::Persistence(msg.into())
    }

    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::Serialization(msg.into())
    }

    pub fn deserialization(msg: impl Into<String>) -> Self {
        Self::Deserialization(msg.into())
    }

    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }
}

// =============================================================================
// 4. Protocol & Identity (Consensus/IdentityError)
// =============================================================================

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConsensusError {
    #[error("Node is not the leader")]
    NotLeader,

    #[error("The leader for the current term is unknown")]
    LeaderUnknown,

    #[error("Consensus proposal at index {0} timed out before reaching quorum")]
    CommitTimeout(LogIndex),

    #[error("Node is in a poisoned state and cannot participate in consensus")]
    Poisoned,

    #[error("Internal consensus engine failure: {0}")]
    Internal(String),

    #[error("Consensus engine is shutting down")]
    Terminated,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("Invalid NodeId format: '{input}' ({source})")]
    InvalidNodeIdFormat {
        input: String,
        #[source]
        source: std::num::ParseIntError,
    },

    #[error("NodeId(0) is reserved and cannot be used for active nodes")]
    ReservedNodeId,

    #[error("ClusterId cannot be empty or consist only of whitespace")]
    EmptyClusterId,

    #[error(
        "ClusterId '{id}' contains invalid characters (must be alphanumeric, dashes, or \
         underscores)"
    )]
    InvalidClusterId { id: String },

    #[error("Invalid ClientId format: {0}")]
    InvalidClientId(String),
}
