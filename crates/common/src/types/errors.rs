use thiserror::Error;

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
pub enum DomainError {
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
