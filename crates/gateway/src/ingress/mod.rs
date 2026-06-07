//! # Ingress Layer (Layer 1-3)
//!
//! This module implements the "Fortress of Entry" (ADR 007), serving as the
//! Ingress Firewall and Defensive Onion pipeline. It handles clinical
//! stabilization of user intent, semantic resolution via the Byzantine AI
//! Oracle, and linearizable consensus proposal.

mod ai_oracle;
mod dispatcher;
mod proposer;
mod scrubber;
mod sequencer;
mod stabilizer;
mod types;

pub use dispatcher::IngressDispatcher;
pub use types::IngressConfig;

#[cfg(test)]
pub(crate) mod test_utils;

#[cfg(test)]
mod tests;
