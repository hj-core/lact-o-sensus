//! Application-specific boundary traits for Lact-O-Sensus.
//!
//! This module defines the interfaces for interacting with the grocery-specific
//! business state and session metadata. By isolating these traits from the
//! generic Raft engine, we maintain strict architectural boundaries.

use std::fmt::Debug;

use crate::proto::v1::app::GroceryItem;
use crate::proto::v1::app::SessionRecord;
use crate::types::ClientId;
use crate::types::LogIndex;
use crate::types::SequenceId;
use crate::types::errors::FsmError;

/// Trait for Exactly-Once session validation (ADR 006).
pub trait SessionProvider: Send + Sync + Debug {
    /// Checks the local session table for Exactly-Once deduplication.
    ///
    /// Providing a `sequence_id` of `0` returns the most recent record for the
    /// client, which allows the Gateway to validate sequence continuity.
    fn check_session(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
    ) -> Result<Option<SessionRecord>, FsmError>;
}

/// Trait for authoritative business state retrieval.
pub trait InventoryReader: Send + Sync + Debug {
    /// Returns the current list of items in the inventory.
    fn get_inventory(&self) -> Vec<GroceryItem>;

    /// Returns the version (LogIndex) that this snapshot represents.
    fn current_version(&self) -> Result<LogIndex, FsmError>;
}
