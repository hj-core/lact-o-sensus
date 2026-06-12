//! Shared foundational types, SI unit registry, taxonomy, slug conventions,
//! protobuf contracts, and gRPC API definitions for the Lact-O-Sensus cluster.
//!
//! All crates depend on `common` for domain primitives (NewTypes), physical
//! quantity normalization, and wire-format message types.

pub mod app_api;
pub mod proto;
pub mod raft_api;
pub mod slug;
pub mod taxonomy;
pub mod types;
pub mod units;
