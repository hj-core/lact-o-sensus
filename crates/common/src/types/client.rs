use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::types::errors::DomainError;

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
