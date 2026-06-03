//! Shared RaftNode foundation and core data types.
//!
//! This module implements the "Physical Foundation" of the Tri-Layer
//! Onion architecture (ADR 009). It defines the `RaftNode` container,
//! shared accessors, and foundational types used across all roles.

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
use tracing::info;

use super::Follower;
use crate::storage::LogStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

// =============================================================================
// 1. Public Snapshots & Types
// =============================================================================

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

pub trait NodeState: Debug {}

// =============================================================================
// 3. Shared Container (RaftNode)
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

    /// Internal helper to transition between roles by recomposing the node
    /// with a new state marker while preserving physical invariants.
    pub(crate) fn transition<Next: NodeState>(self, next_state: Next) -> RaftNode<Next, S> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();
        RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: next_state,
        }
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

    pub(crate) fn truncate_log(&mut self, index: LogIndex) -> Result<(), NodeError> {
        self.log_store.truncate_log(index).map_err(NodeError::from)
    }

    pub(crate) fn append_entries(&mut self, entries: Vec<LogEntry>) -> Result<(), NodeError> {
        self.log_store
            .append_entries(entries)
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
        let mut node = self.transition(Follower::new(leader_id, last_heartbeat, timeout));

        // Transition to next term if higher (§5.1)
        node.advance_term(term)?;

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            leader_id = ?leader_id,
            "Role Transition: -> Follower"
        );

        Ok(node)
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
    pub(crate) fn into_parts(
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
