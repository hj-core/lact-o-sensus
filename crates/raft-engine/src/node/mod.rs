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

use std::fmt::Debug;
use std::sync::Arc;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;

use crate::storage::LogStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

pub mod candidate;
pub mod follower;
pub mod leader;

pub use candidate::*;
pub use follower::*;
pub use leader::*;

// =============================================================================
// 1. Public Snapshots & Types
// =============================================================================

/// The result of a physical log reconciliation operation (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciliationResult {
    pub success: bool,
    pub last_index: LogIndex,
}

impl ReconciliationResult {
    pub fn success(last_index: LogIndex) -> Self {
        Self {
            success: true,
            last_index,
        }
    }

    pub fn mismatch(last_index: LogIndex) -> Self {
        Self {
            success: false,
            last_index,
        }
    }
}

/// Semantic instructions for the deterministic Tick Loop.
///
/// Instructs the execution shell on whether to trigger an election,
/// send a heartbeat, or halt due to a safety violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickAction {
    /// No significant event; continue ticking.
    None,
    /// Election timeout reached; transition to Candidate and campaign.
    StartElection,
    /// Heartbeat interval reached; send AppendEntries to all peers.
    SendHeartbeat,
    /// Terminal state reached; stop the tick loop (ADR 009).
    Stop,
}

// =============================================================================
// 2. Role Markers (Type-State Engine)
// =============================================================================

/// Passive role responsible for log reconciliation and heartbeat tracking.
///
/// Follower nodes maintain liveness by monitoring heartbeats from the leader
/// (ADR 003). If the heartbeat timer expires, the node transitions to the
/// Candidate role to initiate an election.

/// Authoritative role responsible for mutation ingestion and log replication.
///
/// The Leader manages the cluster's logical timeline by replicating log
/// entries to Followers and advancing the commit index once a quorum has
/// acknowledged reception (§5.3).

pub trait NodeState: Debug {}

// =============================================================================
// 3. Shared Shared Container (RaftNode)
// =============================================================================

/// Container for Raft state that is shared across all roles or must persist.
///
/// Represents the "Physical Node" layer, managing log log_store, term
/// persistence, and the commitment boundary.
///
/// SILENT STATE MACHINE: This struct is a pure data mutator. It does NOT own
/// signaling channels or perform I/O. Signaling is the responsibility of the
/// high-level orchestrator shell.
#[derive(Debug)]
pub struct RaftNode<R: NodeState, S: StateMachine> {
    pub(super) identity: Arc<NodeIdentity>,
    pub(super) fsm: Arc<S>,
    pub(super) log_store: Arc<dyn LogStorage>,

    // --- Volatile State ---
    pub(super) last_committed: LogIndex,
    pub(super) last_applied: LogIndex,
    pub(super) state: R,
}

// --- Implementation: Shared Accessors ---

impl<R: NodeState, S: StateMachine> RaftNode<R, S> {
    pub fn cluster_id(&self) -> &ClusterId {
        self.identity.cluster_id()
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    pub fn identity(&self) -> Arc<NodeIdentity> {
        self.identity.clone()
    }

    pub fn fsm(&self) -> Arc<S> {
        self.fsm.clone()
    }

    pub fn log_store(&self) -> &Arc<dyn LogStorage> {
        &self.log_store
    }

    pub fn save_snapshot_metadata(&mut self, index: LogIndex, term: Term) -> Result<(), NodeError> {
        self.log_store
            .save_snapshot_metadata(index, term)
            .map_err(NodeError::from)
    }

    pub fn last_included_index(&self) -> Result<LogIndex, NodeError> {
        self.log_store
            .last_included_index()
            .map_err(NodeError::from)
    }

    pub fn last_included_term(&self) -> Result<Term, NodeError> {
        self.log_store.last_included_term().map_err(NodeError::from)
    }

    pub fn current_term(&self) -> Result<Term, NodeError> {
        self.log_store.current_term().map_err(NodeError::from)
    }

    pub fn voted_for(&self) -> Result<Option<NodeId>, NodeError> {
        self.log_store.voted_for().map_err(NodeError::from)
    }

    pub fn last_committed(&self) -> LogIndex {
        self.last_committed
    }

    pub fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    pub fn state(&self) -> &R {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut R {
        &mut self.state
    }

    // --- Log Queries ---

    pub fn last_log_index(&self) -> Result<LogIndex, NodeError> {
        self.log_store.last_log_index().map_err(NodeError::from)
    }

    pub fn last_log_term(&self) -> Result<Term, NodeError> {
        self.log_store.last_log_term().map_err(NodeError::from)
    }

    pub fn get_term_at(&self, index: LogIndex) -> Result<Term, NodeError> {
        if index == LogIndex::ZERO {
            return Ok(Term::ZERO);
        }

        // §7: Snapshot metadata is authoritative for truncated history.
        let last_included = self
            .log_store
            .last_included_index()
            .map_err(NodeError::from)?;
        if index == last_included {
            return self.log_store.last_included_term().map_err(NodeError::from);
        }

        // Fallback to physical log for indices beyond the snapshot horizon.
        self.log_store
            .read_entry(index)
            .map_err(NodeError::from)
            .map(|opt| opt.map(|e| Term::new(e.term)).unwrap_or(Term::ZERO))
    }

    pub(crate) fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, NodeError> {
        self.log_store
            .read_entries(start, end)
            .map_err(NodeError::from)
    }
}

// --- Implementation: Shared Physical Mutations ---

impl<R: NodeState, S: StateMachine> RaftNode<R, S> {
    /// Consumes the current node and returns it in a Follower role.
    /// This is the primary mechanism for demotion and term updates.
    ///
    /// NOTE: This is a pure factory transformation.
    pub fn try_into_follower(
        self,
        term: Term,
        leader_id: Option<NodeId>,
        last_heartbeat: Tick,
        timeout: TickDuration,
    ) -> Result<RaftNode<Follower, S>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Follower::new(leader_id, last_heartbeat, timeout),
        };

        // Transition to next term if higher (§5.1)
        node.advance_term(term)?;

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            leader_id = ?leader_id,
            "Role Transition: -> Follower"
        );

        Ok(node)
    }

    /// Updates the commit index and triggers the application of entries to the
    /// FSM.
    #[instrument(
        name = "advance_commit_index",
        target = "raft::replication",
        skip_all,
        fields(index = %index)
    )]
    pub fn advance_last_committed(&mut self, index: LogIndex) -> Result<(), NodeError> {
        self.update_commit_index_only(index)?;
        self.apply_to_state_machine()?;
        Ok(())
    }

    /// Persists and updates the commit index without triggering FSM
    /// application. Used by the Freeze-Apply mechanism (ADR 011).
    pub fn update_commit_index_only(&mut self, index: LogIndex) -> Result<(), NodeError> {
        if index < self.last_committed {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                "Ignoring stale last_committed update: {} < current {}",
                index, self.last_committed
            );
            return Ok(());
        }

        let last_idx = self.last_log_index()?;
        if index > last_idx {
            return Err(NodeError::Protocol(format!(
                "Attempted to commit index {} but last_log_index is {}",
                index, last_idx
            )));
        }

        if index > self.last_committed {
            // Persist last_committed BEFORE applying to FSM to ensure safety
            // across crashes.
            self.log_store
                .save_last_committed(index)
                .map_err(NodeError::from)?;

            self.last_committed = index;

            info!(
                target: ClinicalTarget::RaftReplication.as_str(),
                index = %index,
                "Commit Index Advanced (Logical Only)"
            );
        }
        Ok(())
    }

    /// Advances both the commit index and the volatile application cache
    /// to a specific horizon after a successful snapshot installation.
    ///
    /// Effectively "jumps" the logical state forward to match the semantic
    /// reality of the restored State Machine.
    pub fn advance_horizon_after_snapshot(&mut self, index: LogIndex) -> Result<(), NodeError> {
        // 1. Advance commit index (and persist to log storage)
        self.update_commit_index_only(index)?;

        // 2. Sync volatile cache
        self.last_applied = index;

        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            index = %index,
            "Logical horizon advanced to match snapshot."
        );

        Ok(())
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    #[instrument(
        name = "fsm_application",
        target = "clinical::fsm",
        skip_all,
        fields(last_committed = %self.last_committed)
    )]
    fn apply_to_state_machine(&mut self) -> Result<(), NodeError> {
        // Safety Barrier: Ensure FSM hasn't regressed or moved ahead of log.
        let fsm_last = self.fsm.last_applied_index().map_err(|e| e.into())?;
        if fsm_last > self.last_committed {
            return Err(NodeError::Protocol(format!(
                "FSM index {} is ahead of last_committed {}. Possible log regression.",
                fsm_last, self.last_committed
            )));
        }

        while self.last_applied < self.last_committed {
            let apply_idx = (self.last_applied + 1)?;
            let entry = self.log_store.read_entry(apply_idx)?.ok_or_else(|| {
                NodeError::Protocol(format!(
                    "Committed entry {} missing from log during apply",
                    apply_idx
                ))
            })?;

            if let Err(e) = self.fsm.apply(apply_idx, &entry.data) {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    index = %apply_idx,
                    error = %e,
                    "State machine failed to apply index. Triggering Halt Mandate."
                );
                return Err(e.into());
            }

            debug!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                index = %apply_idx,
                "Physical Mutation Resolved"
            );

            self.last_applied = apply_idx;
        }
        Ok(())
    }

    /// Updates the current term and resets voting state if the term increased.
    pub(crate) fn advance_term(&mut self, term: Term) -> Result<(), NodeError> {
        let current = self.log_store.current_term()?;
        if term < current {
            return Err(NodeError::Protocol(format!(
                "Term regression detected! current={} new={}",
                current, term
            )));
        }
        if term > current {
            self.log_store
                .save_hard_state(term, None)
                .map_err(NodeError::from)?;

            info!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                current_term = %current,
                new_term = %term,
                "Term Advanced"
            );
        }
        Ok(())
    }

    pub fn persist_vote(&mut self, candidate_id: NodeId) -> Result<(), NodeError> {
        let term = self.log_store.current_term()?;
        self.log_store
            .save_hard_state(term, Some(candidate_id))
            .map_err(NodeError::from)?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn into_parts(
        self,
    ) -> (
        Arc<NodeIdentity>,
        Arc<S>,
        Arc<dyn LogStorage>,
        LogIndex,
        LogIndex,
    ) {
        (
            self.identity,
            self.fsm,
            self.log_store,
            self.last_committed,
            self.last_applied,
        )
    }
}

// =============================================================================
// 7. Role Marker Boilerplate
// =============================================================================

// =============================================================================
// 8. Behavioral Specification (Tests)
// =============================================================================

#[cfg(test)]
mod tests;
