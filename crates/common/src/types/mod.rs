//! Self-validating domain primitives (NewTypes) for identity, consensus,
//! client tracking, error classification, and clinical telemetry.
//!
//! Each type enforces its invariants at construction and provides controlled
//! conversions. Re-exports the most commonly used types at the module root.

pub mod client;
pub mod consensus;
pub mod errors;
pub mod identity;
pub mod trace;

pub use client::ClientId;
pub use consensus::LogIndex;
pub use consensus::SequenceId;
pub use consensus::Term;
pub use errors::IdentityError;
pub use identity::ClusterId;
pub use identity::NodeId;
pub use identity::NodeIdentity;
pub use trace::TraceId;
