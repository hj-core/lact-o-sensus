//! Physical node foundation and Type-State orchestrator for the Raft engine.
//!
//! This module implements the "Physical Foundation" layer of the Tri-Layer
//! Onion architecture (ADR 009). It manages the core Raft state (Term, Log,
//! Commit Index) and enforces role-based behavioral transitions using the
//! Type-State pattern.
//!
//! The `RaftNode` struct acts as a pure data mutator and invariant protector,
//! delegating I/O and asynchronous signaling to the high-level
//! `ConsensusShell`.

pub mod candidate;
pub mod follower;
pub mod fsm_ops;
pub mod leader;
pub mod pre_candidate;
pub mod shared;

#[cfg(test)]
mod shared_tests;
#[cfg(test)]
pub(crate) mod test_utils;

pub use candidate::*;
pub use follower::*;
pub use leader::*;
pub use pre_candidate::*;
pub use shared::*;
