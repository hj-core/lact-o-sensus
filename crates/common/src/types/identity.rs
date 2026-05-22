//! Identity types for the Lact-O-Sensus cluster.
//!
//! This module provides the foundational identifiers used to distinguish nodes
//! and clusters within the system. These types are implemented as
//! self-validating NewTypes to prevent primitive obsession and ensure
//! architectural invariants are maintained from the moment of construction.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::types::errors::IdentityError;

/// Unique identifier for a node within a cluster.
///
/// NodeId(0) is reserved as a sentinel value and cannot be used for active
/// nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64")]
pub struct NodeId(u64);

impl NodeId {
    /// Constructs a new NodeId. Returns an error if the ID is 0.
    pub fn try_new(id: u64) -> Result<Self, IdentityError> {
        if id == 0 {
            return Err(IdentityError::ReservedNodeId);
        }
        Ok(Self(id))
    }

    /// Returns the underlying primitive value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for NodeId {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s
            .parse::<u64>()
            .map_err(|e| IdentityError::InvalidNodeIdFormat {
                input: s.to_string(),
                source: e,
            })?;
        Self::try_new(id)
    }
}

impl TryFrom<u64> for NodeId {
    type Error = IdentityError;

    fn try_from(id: u64) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

impl TryFrom<String> for NodeId {
    type Error = IdentityError;

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
    pub fn try_new(id: impl AsRef<str>) -> Result<Self, IdentityError> {
        let trimmed = id.as_ref().trim();
        if trimmed.is_empty() {
            return Err(IdentityError::EmptyClusterId);
        }

        if !trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(IdentityError::InvalidClusterId {
                id: trimmed.to_string(),
            });
        }

        Ok(Self(trimmed.to_string()))
    }

    /// Returns the underlying string slice.
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
    type Error = IdentityError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

impl TryFrom<&str> for ClusterId {
    type Error = IdentityError;

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
    /// Constructs a new NodeIdentity from the given cluster and node
    /// identifiers.
    pub fn new(cluster_id: ClusterId, node_id: NodeId) -> Self {
        Self {
            cluster_id,
            node_id,
        }
    }

    /// Returns a reference to the cluster identifier.
    pub fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    /// Returns the node identifier.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod node_id {
        use super::*;

        mod try_new {
            use super::*;

            mod with_valid_input {
                use super::*;
                #[test]
                fn returns_success_when_id_is_positive() {
                    let id = NodeId::try_new(1);
                    assert!(id.is_ok());
                    assert_eq!(id.unwrap().as_u64(), 1);
                }
            }

            mod with_invalid_input {
                use super::*;
                #[test]
                fn returns_error_when_id_is_zero() {
                    let id = NodeId::try_new(0);
                    assert_eq!(id.unwrap_err(), IdentityError::ReservedNodeId);
                }
            }
        }

        mod from_str {
            use super::*;

            mod with_valid_string {
                use super::*;
                #[test]
                fn returns_node_id_when_string_is_numeric() {
                    let id: NodeId = "42".parse().unwrap();
                    assert_eq!(id.as_u64(), 42);
                }
            }

            mod with_invalid_string {
                use super::*;
                #[test]
                fn returns_error_when_string_is_non_numeric() {
                    let result: Result<NodeId, _> = "abc".parse();
                    assert!(matches!(
                        result.unwrap_err(),
                        IdentityError::InvalidNodeIdFormat { .. }
                    ));
                }

                #[test]
                fn returns_error_when_string_is_zero() {
                    let result: Result<NodeId, _> = "0".parse();
                    assert_eq!(result.unwrap_err(), IdentityError::ReservedNodeId);
                }
            }
        }
    }

    mod cluster_id {
        use super::*;

        mod try_new {
            use super::*;

            mod with_valid_input {
                use super::*;
                #[test]
                fn returns_success_when_id_is_alphanumeric() {
                    assert!(ClusterId::try_new("lacto-prod_01").is_ok());
                }

                #[test]
                fn returns_success_and_trims_whitespace() {
                    let id = ClusterId::try_new("  my-cluster  ").unwrap();
                    assert_eq!(id.as_str(), "my-cluster");
                }
            }

            mod with_invalid_input {
                use super::*;
                #[test]
                fn returns_error_when_id_is_empty() {
                    assert_eq!(
                        ClusterId::try_new("  ").unwrap_err(),
                        IdentityError::EmptyClusterId
                    );
                }

                #[test]
                fn returns_error_when_id_contains_special_characters() {
                    let id = "cluster!@#";
                    let result = ClusterId::try_new(id);
                    assert!(matches!(
                        result.unwrap_err(),
                        IdentityError::InvalidClusterId { .. }
                    ));
                }
            }
        }
    }
}
