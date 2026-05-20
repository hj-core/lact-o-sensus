use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::types::errors::IdentityError;

/// Distributed Trace Identifier (ADR 010).
/// Utilizes UUID v7 for chronological sortability across node boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(Uuid);

impl TraceId {
    /// Generates a new authoritative TraceId (Clinical Birth).
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

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
