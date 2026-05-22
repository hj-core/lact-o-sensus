use thiserror::Error;

use crate::types::LogIndex;
use crate::types::NodeId;

// =============================================================================
// 1. Invariant Primitives (Leaf Errors)
// =============================================================================

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArithmeticError {
    #[error("Consensus Invariant Violation: Arithmetic overflow in {type_name}")]
    Overflow { type_name: &'static str },

    #[error("Consensus Invariant Violation: Arithmetic underflow in {type_name}")]
    Underflow { type_name: &'static str },
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

    #[error("Invalid TraceId format: {0}")]
    InvalidTraceId(String),
}

// =============================================================================
// 2. Foundation Layers (Physical & Logical)
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

    #[error("Arithmetic failure: {0}")]
    Arithmetic(#[from] ArithmeticError),
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

    #[error("Arithmetic failure: {0}")]
    Arithmetic(#[from] ArithmeticError),
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
// 3. Protocol Orchestration
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

// =============================================================================
// 4. Composition Root (NodeError)
// =============================================================================

/// Categorical system errors that trigger the Halt Mandate (ADR 009).
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("Physical Storage Failure (Retriable): {0}")]
    Physical(String),

    #[error("Routing Mismatch: I am not the leader (Redirection Hint: {leader_hint:?})")]
    NotLeader { leader_hint: Option<NodeId> },

    #[error("Protocol Invariant Violation (Fatal): {0}")]
    Protocol(String),

    #[error("Data Integrity Violation (Fatal): {0}")]
    Integrity(String),

    #[error("Arithmetic Failure (Fatal): {0}")]
    Arithmetic(#[from] ArithmeticError),
}

impl NodeError {
    /// Returns true if this error triggers the Halt Mandate (ADR 009).
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Protocol(_) | Self::Integrity(_) | Self::Arithmetic(_)
        )
    }
}

impl From<LogStorageError> for NodeError {
    fn from(err: LogStorageError) -> Self {
        match err {
            LogStorageError::Persistence(msg) => NodeError::Physical(msg),
            LogStorageError::Serialization(msg) => NodeError::Integrity(msg),
            LogStorageError::Deserialization(msg) => NodeError::Integrity(msg),
            LogStorageError::Invariant(msg) => NodeError::Protocol(msg),
            LogStorageError::Arithmetic(e) => NodeError::Arithmetic(e),
        }
    }
}

impl From<FsmError> for NodeError {
    fn from(err: FsmError) -> Self {
        match err {
            FsmError::Persistence(msg) => NodeError::Physical(msg),
            FsmError::Serialization(msg) => NodeError::Integrity(msg),
            FsmError::Deserialization(msg) => NodeError::Integrity(msg),
            FsmError::Invariant(msg) => NodeError::Protocol(msg),
            FsmError::Arithmetic(e) => NodeError::Arithmetic(e),
        }
    }
}

// =============================================================================
// 5. Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    mod node_error {
        use super::*;

        #[test]
        fn identifies_fatal_errors_for_halt_mandate() {
            // Fatal variants
            assert!(NodeError::Protocol("fail".into()).is_fatal());
            assert!(NodeError::Integrity("fail".into()).is_fatal());
            assert!(
                NodeError::Arithmetic(ArithmeticError::Overflow { type_name: "Test" }).is_fatal()
            );

            // Non-fatal variants
            assert!(!NodeError::Physical("fail".into()).is_fatal());
            assert!(!NodeError::NotLeader { leader_hint: None }.is_fatal());
        }

        mod conversions {
            use super::*;

            #[test]
            fn converts_from_log_storage_error() {
                let err = LogStorageError::Persistence("disk full".into());
                let node_err: NodeError = err.into();
                assert!(matches!(node_err, NodeError::Physical(m) if m == "disk full"));

                let err = LogStorageError::Arithmetic(ArithmeticError::Underflow {
                    type_name: "LogIndex",
                });
                let node_err: NodeError = err.into();
                assert!(matches!(
                    node_err,
                    NodeError::Arithmetic(ArithmeticError::Underflow { .. })
                ));
            }

            #[test]
            fn converts_from_fsm_error() {
                let err = FsmError::Invariant("causal gap".into());
                let node_err: NodeError = err.into();
                assert!(matches!(node_err, NodeError::Protocol(m) if m == "causal gap"));
            }
        }
    }

    mod identity_error {
        use super::*;

        #[test]
        fn formats_display_strings_correctly() {
            let err = IdentityError::ReservedNodeId;
            assert_eq!(
                format!("{}", err),
                "NodeId(0) is reserved and cannot be used for active nodes"
            );

            let err = IdentityError::InvalidClusterId { id: "!!!".into() };
            assert!(format!("{}", err).contains("contains invalid characters"));
        }
    }
}
