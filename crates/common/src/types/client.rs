//! Client identity types for the Lact-O-Sensus cluster.
//!
//! This module provides the `ClientId` type, which is used to uniquely identify
//! sessions and ensure exactly-once semantics. To comply with clinical safety
//! and privacy standards (ADR 010), this type enforces PII redaction by
//! truncating identifiers in human-readable output while maintaining full
//! fidelity for consensus and serialization.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::types::errors::IdentityError;

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
    ///
    /// This returns the full, non-redacted UUID string.
    pub fn as_str(&self) -> &str {
        &self.cached_str
    }

    /// Returns a correlation-safe truncation (first 8 characters) for clinical
    /// logging (ADR 010).
    pub fn truncated(&self) -> &str {
        let s = self.as_str();
        if s.len() <= 8 { s } else { &s[..8] }
    }
}

impl FromStr for ClientId {
    type Err = IdentityError;

    /// Parses a ClientId from a UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let inner =
            Uuid::parse_str(s).map_err(|e| IdentityError::InvalidClientId(e.to_string()))?;
        Ok(Self {
            inner,
            cached_str: Arc::from(inner.to_string()),
        })
    }
}

impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted for security (correlation-safe truncation)
        write!(f, "ClientId({}...) ", self.truncated())
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted for security (correlation-safe truncation)
        // We omit the ellipses to strictly follow the 8-character mandate for
        // structured logs.
        write!(f, "{}", self.truncated())
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

#[cfg(test)]
mod tests {
    use super::*;

    mod client_id {
        use super::*;

        mod generate {
            use super::*;
            #[test]
            fn returns_unique_identifiers_on_successive_calls() {
                let id1 = ClientId::generate();
                let id2 = ClientId::generate();
                assert_ne!(id1, id2);
            }
        }

        mod from_str {
            use super::*;

            mod with_valid_uuid {
                use super::*;
                #[test]
                fn returns_success_when_format_is_correct() {
                    let raw = "550e8400-e29b-41d4-a716-446655440000";
                    let id = ClientId::from_str(raw);
                    assert!(id.is_ok());
                    assert_eq!(id.unwrap().as_str(), raw);
                }
            }

            mod with_invalid_input {
                use super::*;
                #[test]
                fn returns_error_when_string_is_not_a_uuid() {
                    let result = ClientId::from_str("not-a-uuid");
                    assert!(matches!(result, Err(IdentityError::InvalidClientId(_))));
                }
            }
        }

        mod redacting_pii {
            use super::*;

            mod via_display {
                use super::*;
                #[test]
                fn truncates_output_to_eight_characters() {
                    let raw = "550e8400-e29b-41d4-a716-446655440000";
                    let id = ClientId::from_str(raw).unwrap();
                    assert_eq!(format!("{}", id), "550e8400");
                }
            }

            mod via_debug {
                use super::*;
                #[test]
                fn includes_ellipses_and_prefix_in_redacted_output() {
                    let raw = "550e8400-e29b-41d4-a716-446655440000";
                    let id = ClientId::from_str(raw).unwrap();
                    assert_eq!(format!("{:?}", id), "ClientId(550e8400...) ");
                }
            }
        }

        mod serialization {
            use super::*;
            #[test]
            fn maintains_full_fidelity_in_round_trip() {
                let original = ClientId::generate();
                let serialized = serde_json::to_string(&original).unwrap();
                let deserialized: ClientId = serde_json::from_str(&serialized).unwrap();
                assert_eq!(original, deserialized);
                assert_eq!(original.as_str(), deserialized.as_str());
            }
        }
    }
}
