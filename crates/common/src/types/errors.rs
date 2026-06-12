//! Domain-specific error types for the Lact-O-Sensus cluster.
//!
//! This module provides a hierarchical error model that aligns with the
//! system's clinical safety mandates. The error types are organized to support
//! the "Halt Mandate" (ADR 009), distinguishing between retriable physical
//! failures and fatal protocol or data integrity violations.
//!
//! The hierarchy follows a top-down Information Hierarchy (Rule 3), starting
//! with the `NodeError` composition root.

use thiserror::Error;

use crate::types::LogIndex;
use crate::types::NodeId;

// =============================================================================
// 1. Composition Root
// =============================================================================

/// Categorical system errors that trigger the Halt Mandate (ADR 009).
///
/// This is the primary error type used by the node engine to determine if
/// a failure requires a transition to the `Poisoned` state before panicking.
#[derive(Debug, Error)]
pub enum NodeError {
    /// A failure in the physical storage layer (e.g. disk full, I/O error).
    /// Usually considered retriable through a node restart.
    #[error("Physical Storage Failure (Retriable): {0}")]
    Physical(String),

    /// A routing mismatch indicating the node is not the current leader.
    /// Includes an optional hint for the known leader's address.
    #[error("Routing Mismatch: I am not the leader (Redirection Hint: {leader_hint:?})")]
    NotLeader {
        /// The identifier of the node believed to be the leader.
        leader_hint: Option<NodeId>,
    },

    /// A violation of the Raft protocol invariants (Fatal).
    #[error("Protocol Invariant Violation (Fatal): {0}")]
    Protocol(String),

    /// A violation of data integrity within the log or FSM (Fatal).
    #[error("Data Integrity Violation (Fatal): {0}")]
    Integrity(String),

    /// An arithmetic overflow or underflow (Fatal).
    #[error("Arithmetic Failure (Fatal): {0}")]
    Arithmetic(#[from] ArithmeticError),
}

impl NodeError {
    /// Returns true if this error triggers the Halt Mandate (ADR 009).
    ///
    /// Fatal errors indicate that the node's logical state has diverged or
    /// corrupted, requiring a transition to `Poisoned` to prevent "Zombie
    /// Node" behavior.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Protocol(_) | Self::Integrity(_) | Self::Arithmetic(_)
        )
    }
}

// =============================================================================
// 2. Protocol Orchestration
// =============================================================================

/// Errors encountered during the consensus replication lifecycle.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConsensusError {
    /// The local node is currently a Follower or Candidate.
    #[error("Node is not the leader")]
    NotLeader,

    /// The leader for the current election term has not been established.
    #[error("The leader for the current term is unknown")]
    LeaderUnknown,

    /// A proposal failed to reach a quorum within the mandatory timeout.
    #[error("Consensus proposal at index {0} timed out before reaching quorum")]
    CommitTimeout(LogIndex),

    /// A consensus operation (e.g. leadership verification) timed out.
    #[error("Consensus operation timed out")]
    Timeout,

    /// The node has encountered a fatal invariant and is poisoned.
    #[error("Node is in a poisoned state and cannot participate in consensus")]
    Poisoned,

    /// An unrecoverable internal failure in the consensus engine.
    #[error("Internal consensus engine failure: {0}")]
    Internal(String),

    /// The consensus loop is shutting down cleanly.
    #[error("Consensus engine is shutting down")]
    Terminated,
}

// =============================================================================
// 3. Foundation Layers (Physical & Logical)
// =============================================================================

/// Errors occurring within the Raft log storage engine.
#[derive(Debug, Error)]
pub enum LogStorageError {
    /// Failure during synchronous disk I/O.
    #[error("Persistence failure: {0}")]
    Persistence(String),

    /// Failure to serialize a log entry for storage.
    #[error("Serialization failure: {0}")]
    Serialization(String),

    /// Failure to deserialize a log entry retrieved from disk.
    #[error("Deserialization failure: {0}")]
    Deserialization(String),

    /// Violation of a foundational storage invariant.
    #[error("Foundation invariant violation: {0}")]
    Invariant(String),

    /// Arithmetic failure during index or term calculation.
    #[error("Arithmetic failure: {0}")]
    Arithmetic(#[from] ArithmeticError),
}

impl LogStorageError {
    /// Factory for creating a Persistence error.
    pub fn persistence(msg: impl Into<String>) -> Self {
        Self::Persistence(msg.into())
    }

    /// Factory for creating a Serialization error.
    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::Serialization(msg.into())
    }

    /// Factory for creating a Deserialization error.
    pub fn deserialization(msg: impl Into<String>) -> Self {
        Self::Deserialization(msg.into())
    }

    /// Factory for creating an Invariant error.
    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }
}

/// Errors occurring within the Replicated State Machine (FSM).
#[derive(Debug, Error)]
pub enum FsmError {
    /// Failure to persist FSM inventory or clinical state.
    #[error("Persistence failure: {0}")]
    Persistence(String),

    /// Failure to serialize state machine data.
    #[error("Serialization failure: {0}")]
    Serialization(String),

    /// Failure to deserialize state machine data.
    #[error("Deserialization failure: {0}")]
    Deserialization(String),

    /// Violation of a foundational state machine invariant.
    #[error("Foundation invariant violation: {0}")]
    Invariant(String),

    /// Arithmetic failure during inventory or sequence tracking.
    #[error("Arithmetic failure: {0}")]
    Arithmetic(#[from] ArithmeticError),

    /// State machine is poisoned due to a prior invariant violation.
    #[error("State machine is poisoned")]
    Poisoned,
}

impl FsmError {
    /// Factory for creating a Persistence error.
    pub fn persistence(msg: impl Into<String>) -> Self {
        Self::Persistence(msg.into())
    }

    /// Factory for creating a Serialization error.
    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::Serialization(msg.into())
    }

    /// Factory for creating a Deserialization error.
    pub fn deserialization(msg: impl Into<String>) -> Self {
        Self::Deserialization(msg.into())
    }

    /// Factory for creating an Invariant error.
    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }

    /// Returns the `Poisoned` sentinel error.
    pub fn poisoned() -> Self {
        Self::Poisoned
    }
}

// =============================================================================
// 4. Invariant Primitives (Leaf Errors)
// =============================================================================

/// Failures in checked arithmetic for monotonic consensus identifiers.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArithmeticError {
    /// A calculation exceeded the maximum bound of the NewType.
    #[error("Consensus Invariant Violation: Arithmetic overflow in {type_name}")]
    Overflow {
        /// The name of the NewType where the overflow occurred.
        type_name: &'static str,
    },

    /// A calculation fell below the zero-bound of the NewType.
    #[error("Consensus Invariant Violation: Arithmetic underflow in {type_name}")]
    Underflow {
        /// The name of the NewType where the underflow occurred.
        type_name: &'static str,
    },
}

/// Failures encountered during the validation of cluster or node identity.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    /// Malformed string representation of a NodeId.
    #[error("Invalid NodeId format: '{input}' ({source})")]
    InvalidNodeIdFormat {
        /// The raw input string that failed to parse.
        input: String,
        /// The underlying parse error.
        #[source]
        source: std::num::ParseIntError,
    },

    /// Attempt to use the reserved NodeId(0).
    #[error("NodeId(0) is reserved and cannot be used for active nodes")]
    ReservedNodeId,

    /// ClusterId was empty or contained only whitespace.
    #[error("ClusterId cannot be empty or consist only of whitespace")]
    EmptyClusterId,

    /// ClusterId contained unauthorized characters.
    #[error(
        "ClusterId '{id}' contains invalid characters (must be alphanumeric, dashes, or \
         underscores)"
    )]
    InvalidClusterId {
        /// The raw identifier string.
        id: String,
    },

    /// malformed string representation of a ClientId.
    #[error("Invalid ClientId format: {0}")]
    InvalidClientId(String),

    /// malformed string representation of a TraceId.
    #[error("Invalid TraceId format: {0}")]
    InvalidTraceId(String),
}

// --- Implementation of Hierarchy Conversions ---

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
            FsmError::Poisoned => NodeError::Protocol("State machine is poisoned".into()),
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

        mod is_fatal {
            use super::*;

            #[test]
            fn returns_true_when_error_violates_protocol_invariants() {
                assert!(NodeError::Protocol("fail".into()).is_fatal());
            }

            #[test]
            fn returns_true_when_error_violates_data_integrity() {
                assert!(NodeError::Integrity("fail".into()).is_fatal());
            }

            #[test]
            fn returns_true_when_error_is_arithmetic_failure() {
                assert!(
                    NodeError::Arithmetic(ArithmeticError::Overflow { type_name: "Test" })
                        .is_fatal()
                );
            }

            #[test]
            fn returns_false_when_error_is_physical_storage_failure() {
                assert!(!NodeError::Physical("fail".into()).is_fatal());
            }

            #[test]
            fn returns_false_when_error_is_routing_mismatch() {
                assert!(!NodeError::NotLeader { leader_hint: None }.is_fatal());
            }
        }

        mod conversions {
            use super::*;

            mod from_log_storage_error {
                use super::*;

                #[test]
                fn maps_persistence_to_physical_error() {
                    let err = LogStorageError::Persistence("disk full".into());
                    let node_err: NodeError = err.into();
                    assert!(matches!(node_err, NodeError::Physical(m) if m == "disk full"));
                }

                #[test]
                fn maps_arithmetic_to_arithmetic_error() {
                    let err = LogStorageError::Arithmetic(ArithmeticError::Underflow {
                        type_name: "LogIndex",
                    });
                    let node_err: NodeError = err.into();
                    assert!(matches!(
                        node_err,
                        NodeError::Arithmetic(ArithmeticError::Underflow { .. })
                    ));
                }
            }

            mod from_fsm_error {
                use super::*;

                #[test]
                fn maps_invariant_to_protocol_error() {
                    let err = FsmError::Invariant("causal gap".into());
                    let node_err: NodeError = err.into();
                    assert!(matches!(node_err, NodeError::Protocol(m) if m == "causal gap"));
                }

                #[test]
                fn maps_poisoned_to_protocol_error() {
                    let node_err: NodeError = FsmError::Poisoned.into();
                    assert!(
                        matches!(node_err, NodeError::Protocol(m) if m == "State machine is poisoned")
                    );
                }
            }
        }
    }

    mod identity_error {
        use super::*;

        mod display {
            use super::*;

            #[test]
            fn formats_reserved_node_id_accurately() {
                let err = IdentityError::ReservedNodeId;
                assert_eq!(
                    format!("{}", err),
                    "NodeId(0) is reserved and cannot be used for active nodes"
                );
            }

            #[test]
            fn formats_invalid_cluster_id_accurately() {
                let err = IdentityError::InvalidClusterId { id: "!!!".into() };
                assert!(format!("{}", err).contains("contains invalid characters"));
            }
        }
    }
}
