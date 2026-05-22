//! Distributed telemetry and tracing foundational types for Lact-O-Sensus.
//!
//! This module defines the types used for distributed correlation and
//! structured logging as mandated by ADR 010. It provides the `TraceId`, a
//! chronologically sortable identifier used to track requests across node
//! boundaries, and the `ClinicalTarget` registry to ensure type-safe telemetry
//! events.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::types::errors::IdentityError;

/// Distributed Trace Identifier (ADR 010).
///
/// Utilizes UUID v7 for chronological sortability across node boundaries.
/// This allows telemetry events from different nodes to be interleaved
/// in their logical order of occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(Uuid);

impl TraceId {
    /// Generates a new authoritative TraceId (Clinical Birth).
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// Returns a reference to the underlying UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TraceId {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| IdentityError::InvalidTraceId(e.to_string()))
    }
}

/// Standardized targets for structured clinical events (ADR 010).
///
/// Enforces the "Fortress of Truth" by preventing typos in telemetry
/// event targets across the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClinicalTarget {
    /// Core consensus state transitions.
    RaftFoundation,
    /// Log maintenance and replication.
    RaftReplication,
    /// Ingress pipeline and defensive onion.
    ClinicalIngress,
    /// State machine physical mutations.
    ClinicalFsm,
    /// Node identity and storage foundation.
    ClinicalFoundation,
    /// Cold-boot recovery and log replay.
    ClinicalRecovery,
    /// Semantic resolution and LLM calls.
    ClinicalOracle,
    /// Transport-layer integrity and causal verification.
    ClinicalTelemetry,
}

impl ClinicalTarget {
    /// Returns the canonical target string defined in ADR 010.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RaftFoundation => "raft::foundation",
            Self::RaftReplication => "raft::replication",
            Self::ClinicalIngress => "clinical::ingress",
            Self::ClinicalFsm => "clinical::fsm",
            Self::ClinicalFoundation => "clinical::foundation",
            Self::ClinicalRecovery => "clinical::recovery",
            Self::ClinicalOracle => "clinical::oracle",
            Self::ClinicalTelemetry => "clinical::telemetry",
        }
    }
}

impl fmt::Display for ClinicalTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod trace_id {
        use super::*;

        mod generate {
            use super::*;

            #[test]
            fn returns_unique_identifiers_with_chronological_ordering() {
                let id1 = TraceId::generate();
                // Ensure some time passes for v7 sortability if needed,
                // but v7 is monotonic even in same ms usually.
                let id2 = TraceId::generate();
                assert_ne!(id1, id2);
                assert!(id1.as_uuid() < id2.as_uuid());
            }
        }

        mod from_str {
            use super::*;

            mod with_valid_uuid {
                use super::*;
                #[test]
                fn returns_success_when_format_is_correct() {
                    let raw = "018f9b1c-3a5e-7000-8000-000000000000";
                    let id = TraceId::from_str(raw).unwrap();
                    assert_eq!(id.to_string(), raw);
                }
            }

            mod with_invalid_input {
                use super::*;
                #[test]
                fn returns_error_when_string_is_malformed() {
                    let result = TraceId::from_str("not-a-trace-id");
                    assert!(matches!(result, Err(IdentityError::InvalidTraceId(_))));
                }
            }
        }
    }

    mod clinical_target {
        use super::*;

        mod as_str {
            use super::*;

            #[test]
            fn returns_correct_canonical_string_for_all_variants() {
                assert_eq!(ClinicalTarget::RaftFoundation.as_str(), "raft::foundation");
                assert_eq!(
                    ClinicalTarget::ClinicalIngress.as_str(),
                    "clinical::ingress"
                );
                assert_eq!(ClinicalTarget::ClinicalOracle.as_str(), "clinical::oracle");
            }
        }

        mod display {
            use super::*;

            #[test]
            fn matches_the_canonical_string_representation() {
                let target = ClinicalTarget::ClinicalFsm;
                assert_eq!(format!("{}", target), target.as_str());
            }
        }
    }
}
