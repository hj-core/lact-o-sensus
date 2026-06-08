//! Clinical Consensus Orchestration
//!
//! This module implements the Raft consensus state machine transitions,
//! orchestrating the deterministic heartbeat, leader elections, and log
//! replication cycles.
//!
//! It acts as the "Logical Orchestrator" within the internal node architecture
//! (ADR 009). All asynchronous network fanning is decoupled from the strictly
//! deterministic logical clock driven by the Tick Loop (ADR 003). To maintain
//! clinical integrity, operations explicitly map distributed responses to
//! internal state mutations while propagating causal telemetry traces (ADR
//! 010).

pub(crate) mod types;
pub(crate) use types::*;

pub(crate) mod election;

pub(crate) mod replication;
pub(crate) use replication::initiate_replication;

pub(crate) mod rpc;

pub(crate) mod lifecycle;
pub use lifecycle::spawn_background_applier;
pub use lifecycle::spawn_tick_loop;

#[cfg(test)]
mod tests;
