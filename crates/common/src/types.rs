use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

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

/// Unique identifier for a specific client session.
///
/// This type uses a UUID internally but provides zero-allocation string access
/// via a cached Arc<str>. Display and Debug implementations are truncated
/// for security.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClientId {
    inner: Uuid,
    cached_str: Arc<str>,
}

impl ClientId {
    /// Generates a new random ClientId.
    pub fn generate() -> Self {
        let inner = Uuid::new_v4();
        Self {
            inner,
            cached_str: Arc::from(inner.to_string()),
        }
    }

    /// Returns a reference to the pre-formatted string representation.
    pub fn as_str(&self) -> &str {
        &self.cached_str
    }
}

impl FromStr for ClientId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let inner = Uuid::parse_str(s).map_err(|e| DomainError::InvalidClientId(e.to_string()))?;
        Ok(Self {
            inner,
            cached_str: Arc::from(inner.to_string()),
        })
    }
}

impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted for security (correlation-safe truncation)
        write!(f, "ClientId({}...) ", &self.as_str()[..8])
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted for security (correlation-safe truncation)
        write!(f, "{}...", &self.as_str()[..8])
    }
}

impl Serialize for ClientId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Unique identifier for a node within a cluster.
///
/// NodeId(0) is reserved as a sentinel value and cannot be used for active
/// nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64")]
pub struct NodeId(u64);

impl NodeId {
    /// Constructs a new NodeId. Returns an error if the ID is 0.
    pub fn try_new(id: u64) -> Result<Self, DomainError> {
        if id == 0 {
            return Err(DomainError::ReservedNodeId);
        }
        Ok(Self(id))
    }

    /// Convenience constructor for tests or known-safe values.
    ///
    /// # Panics
    /// Panics if the id is 0.
    pub fn new(id: u64) -> Self {
        Self::try_new(id).expect("NodeId cannot be 0")
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for NodeId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s
            .parse::<u64>()
            .map_err(|e| DomainError::InvalidNodeIdFormat {
                input: s.to_string(),
                source: e,
            })?;
        Self::try_new(id)
    }
}

impl TryFrom<u64> for NodeId {
    type Error = DomainError;

    fn try_from(id: u64) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

impl TryFrom<String> for NodeId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Unique identifier for an entire consensus group.
///
/// This type is self-validating: it must be non-empty and contain only
/// alphanumeric characters, dashes, or underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ClusterId(String);

impl ClusterId {
    /// Constructs a new ClusterId from a string-like type.
    ///
    /// Trims whitespace and verifies it is not empty and contains valid
    /// characters.
    pub fn try_new(id: impl AsRef<str>) -> Result<Self, DomainError> {
        let trimmed = id.as_ref().trim();
        if trimmed.is_empty() {
            return Err(DomainError::EmptyClusterId);
        }

        if !trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(DomainError::InvalidClusterId {
                id: trimmed.to_string(),
            });
        }

        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for ClusterId {
    type Error = DomainError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

impl TryFrom<&str> for ClusterId {
    type Error = DomainError;

    fn try_from(id: &str) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

/// Persistent logical identity of a node within a cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeIdentity {
    cluster_id: ClusterId,
    node_id: NodeId,
}

impl NodeIdentity {
    pub fn new(cluster_id: ClusterId, node_id: NodeId) -> Self {
        Self {
            cluster_id,
            node_id,
        }
    }

    pub fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }
}

macro_rules! define_u64_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(val: u64) -> Self {
                Self(val)
            }

            pub fn value(&self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<u64> for $name {
            fn from(val: u64) -> Self {
                Self(val)
            }
        }

        impl From<$name> for u64 {
            fn from(val: $name) -> u64 {
                val.0
            }
        }

        impl std::ops::Add<u64> for $name {
            type Output = Self;

            fn add(self, rhs: u64) -> Self {
                Self(
                    self.0
                        .checked_add(rhs)
                        .expect("Halt Mandate: Arithmetic overflow in domain type"),
                )
            }
        }
    };
}

define_u64_newtype!(LogIndex, "Monotonic index of an entry in the Raft log.");
define_u64_newtype!(Term, "The current election term in the Raft cluster.");
define_u64_newtype!(
    SequenceId,
    "Monotonic sequence identifier for Exactly-Once Semantics (EOS)."
);

// --- Domain-Specific Arithmetic Overrides ---

impl std::ops::Sub<u64> for LogIndex {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self {
        Self(
            self.0
                .checked_sub(rhs)
                .expect("Halt Mandate: LogIndex underflow (protocol violation)"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod node_id {
        use super::*;

        mod try_new {
            use super::*;
            #[test]
            fn accepts_valid_id() {
                let id = NodeId::try_new(1);
                assert!(id.is_ok());
                assert_eq!(id.unwrap().value(), 1);
            }

            #[test]
            fn rejects_reserved_zero() {
                let id = NodeId::try_new(0);
                assert_eq!(id.unwrap_err(), DomainError::ReservedNodeId);
            }
        }

        mod from_str {
            use super::*;
            #[test]
            fn parses_valid_string() {
                let id: NodeId = "42".parse().unwrap();
                assert_eq!(id.value(), 42);
            }

            #[test]
            fn fails_on_invalid_string() {
                let result: Result<NodeId, _> = "abc".parse();
                assert!(matches!(
                    result.unwrap_err(),
                    DomainError::InvalidNodeIdFormat { .. }
                ));
            }

            #[test]
            fn fails_on_zero_string() {
                let result: Result<NodeId, _> = "0".parse();
                assert_eq!(result.unwrap_err(), DomainError::ReservedNodeId);
            }
        }
    }

    mod cluster_id {
        use super::*;

        mod try_new {
            use super::*;
            #[test]
            fn accepts_valid_id() {
                assert!(ClusterId::try_new("lacto-prod_01").is_ok());
            }

            #[test]
            fn rejects_empty_string() {
                assert_eq!(
                    ClusterId::try_new("  ").unwrap_err(),
                    DomainError::EmptyClusterId
                );
            }

            #[test]
            fn rejects_invalid_characters() {
                let id = "cluster!@#";
                let result = ClusterId::try_new(id);
                assert!(matches!(
                    result.unwrap_err(),
                    DomainError::InvalidClusterId { .. }
                ));
            }

            #[test]
            fn trims_whitespace() {
                let id = ClusterId::try_new("  my-cluster  ").unwrap();
                assert_eq!(id.as_str(), "my-cluster");
            }
        }
    }

    mod log_index {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let idx = LogIndex::new(42);
                assert_eq!(idx.value(), 42);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let idx = LogIndex::new(42);
                assert_eq!(format!("{}", idx), "42");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 42u64;
                let idx = LogIndex::from(raw);
                assert_eq!(u64::from(idx), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let idx = LogIndex::new(10);
                let result = idx + 5;
                assert_eq!(result.value(), 15);
            }

            #[test]
            fn supports_subtraction_with_u64() {
                let idx = LogIndex::new(10);
                let result = idx - 3;
                assert_eq!(result.value(), 7);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let idx = LogIndex::new(u64::MAX);
                let _ = idx + 1;
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: LogIndex underflow")]
            fn panics_on_underflow() {
                let idx = LogIndex::new(0);
                let _ = idx - 1;
            }
        }
    }

    mod term {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let term = Term::new(5);
                assert_eq!(term.value(), 5);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let term = Term::new(5);
                assert_eq!(format!("{}", term), "5");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 5u64;
                let term = Term::from(raw);
                assert_eq!(u64::from(term), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let term = Term::new(10);
                let result = term + 1;
                assert_eq!(result.value(), 11);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let term = Term::new(u64::MAX);
                let _ = term + 1;
            }
        }
    }

    mod sequence_id {
        use super::*;

        mod new {
            use super::*;
            #[test]
            fn stores_provided_u64() {
                let seq = SequenceId::new(100);
                assert_eq!(seq.value(), 100);
            }
        }

        mod display {
            use super::*;
            #[test]
            fn formats_as_raw_u64_string() {
                let seq = SequenceId::new(100);
                assert_eq!(format!("{}", seq), "100");
            }
        }

        mod conversions {
            use super::*;
            #[test]
            fn supports_lossless_roundtrip_with_u64() {
                let raw = 100u64;
                let seq = SequenceId::from(raw);
                assert_eq!(u64::from(seq), raw);
            }
        }

        mod arithmetic {
            use super::*;
            #[test]
            fn supports_addition_with_u64() {
                let seq = SequenceId::new(10);
                let result = seq + 1;
                assert_eq!(result.value(), 11);
            }

            #[test]
            #[should_panic(expected = "Halt Mandate: Arithmetic overflow")]
            fn panics_on_overflow() {
                let seq = SequenceId::new(u64::MAX);
                let _ = seq + 1;
            }
        }
    }
}
