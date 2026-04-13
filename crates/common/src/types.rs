use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use super::*;

    mod node_id_try_new {
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

    mod node_id_from_str {
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

    mod cluster_id_try_new {
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
