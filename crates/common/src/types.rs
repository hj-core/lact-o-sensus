use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

/// Unique identifier for a node within a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(u64);

impl NodeId {
    pub fn new(id: u64) -> Self {
        Self(id)
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

impl From<u64> for NodeId {
    fn from(id: u64) -> Self {
        Self::new(id)
    }
}

impl FromStr for NodeId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s.parse::<u64>()?;
        Ok(Self::new(id))
    }
}

impl TryFrom<String> for NodeId {
    type Error = std::num::ParseIntError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Unique identifier for an entire consensus group.
///
/// This type is self-validating: it can only be constructed if the ID is
/// non-empty. Serde deserialization is also guarded by this validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ClusterId(String);

impl ClusterId {
    /// Constructs a new ClusterId from a string-like type.
    ///
    /// Trims whitespace and verifies it is not empty. Avoids allocation
    /// if the validation fails.
    pub fn try_new(id: impl AsRef<str>) -> Result<Self, String> {
        let trimmed = id.as_ref().trim();
        if trimmed.is_empty() {
            return Err("ClusterId cannot be empty or only whitespace".to_string());
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
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}

impl TryFrom<&str> for ClusterId {
    type Error = String;

    fn try_from(id: &str) -> Result<Self, Self::Error> {
        Self::try_new(id)
    }
}
