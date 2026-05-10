use thiserror::Error;

use crate::types::LogIndex;

#[derive(Debug, Error)]
pub enum FsmError {
    #[error("Physical persistence failure: {0}")]
    Persistence(String),

    #[error("Internal encoding failure: {0}")]
    Serialization(String),

    #[error("Data corruption or decoding failure: {0}")]
    Deserialization(String),

    #[error("Foundation invariant violation: {0}")]
    Invariant(String),
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
