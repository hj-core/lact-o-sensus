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

#[cfg(test)]
pub(crate) mod test_utils;

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
    identity: Arc<NodeIdentity>,
    fsm: Arc<S>,
    log_store: Arc<dyn LogStorage>,

    // --- Volatile State ---
    last_committed: LogIndex,
    last_applied: LogIndex,
    state: R,
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
mod tests {
    use super::test_utils::*;
    use super::*;
    use crate::storage::LogStorage;
    use crate::storage::MemoryStorage;
    use common::proto::v1::raft::LogEntry;
    use common::types::errors::FsmError;
    use common::types::errors::NodeError;
    use std::sync::Arc;

    mod shared {
        use super::*;

        mod get_term_at {
            use super::*;

            #[test]
            fn should_return_zero_when_index_is_zero() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let node = setup_node_as_follower(fsm, log_store);
                assert_eq!(node.get_term_at(LogIndex::ZERO).unwrap(), Term::ZERO);
            }

            #[test]
            fn should_return_term_from_snapshot_when_index_is_truncated() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());

                // Setup snapshot at index 10, term 2
                let snapshot_index = LogIndex::new(10);
                let snapshot_term = Term::new(2);
                log_store
                    .save_snapshot_metadata(snapshot_index, snapshot_term)
                    .unwrap();

                let node = setup_node_as_follower(fsm, log_store);

                // Authority check (§7): Should return snapshot term even if log is empty
                assert_eq!(node.get_term_at(snapshot_index).unwrap(), snapshot_term);
            }

            #[test]
            fn should_return_term_from_log_when_index_is_recent() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());

                // Append an entry at index 1, term 1
                log_store
                    .append_entries(vec![LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    }])
                    .unwrap();

                let node = setup_node_as_follower(fsm, log_store);

                assert_eq!(node.get_term_at(LogIndex::new(1)).unwrap(), Term::new(1));
            }
        }

        mod state_mut {
            use super::*;

            mod heartbeat_timer {
                use super::*;

                mod reset {
                    use super::*;

                    #[test]
                    fn should_update_timer_when_invoked() {
                        let fsm = Arc::new(MockFsm::default());
                        let log_store = Arc::new(MemoryStorage::new());
                        let mut node = RaftNode::<Follower, MockFsm>::try_new(
                            test_identity(1),
                            fsm,
                            log_store,
                            Tick::new(0),
                            TickDuration::new(100),
                        )
                        .unwrap();
                        let initial_time = node.state().last_heartbeat();

                        node.state_mut().reset_heartbeat(Tick::new(10));

                        assert_eq!(node.state().last_heartbeat(), Tick::new(10));
                        assert!(node.state().last_heartbeat() > initial_time);
                    }
                }
            }
        }

        mod try_into_follower {
            use super::*;

            mod on_term_update {
                use super::*;

                #[test]
                fn should_reset_voting_state_when_demoted_on_new_term() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::try_new(1).unwrap()))
                        .unwrap();
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let demoted = node
                        .try_into_follower(Term::new(2), None, Tick::new(0), TickDuration::new(100))
                        .unwrap();

                    assert_eq!(demoted.current_term().unwrap(), Term::new(2));
                    assert_eq!(demoted.voted_for().unwrap(), None);
                }

                #[test]
                fn should_preserve_vote_when_demoted_on_same_term() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::try_new(3).unwrap()))
                        .unwrap();
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let demoted = node
                        .try_into_follower(Term::new(1), None, Tick::new(0), TickDuration::new(100))
                        .unwrap();
                    assert_eq!(
                        demoted.voted_for().unwrap(),
                        Some(NodeId::try_new(3).unwrap())
                    );
                }
            }
        }

        mod advance_last_committed {
            use super::*;

            mod on_valid_index {
                use super::*;

                async fn check_persists_to_log_store_when_index_is_valid<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();

                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    assert_eq!(log_store.last_committed().unwrap(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn should_persist_to_log_store_when_index_is_valid_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                async fn check_applies_to_fsm_when_index_is_valid<R: NodeState, S: StateMachine>(
                    mut node: RaftNode<R, S>,
                    fsm: Arc<MockFsm>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![1],
                        }])
                        .unwrap();

                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(1));
                    assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
                }

                #[tokio::test]
                async fn should_apply_to_fsm_when_index_is_valid_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn should_apply_to_fsm_when_index_is_valid_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn should_apply_to_fsm_when_index_is_valid_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                async fn check_applies_multiple_entries_sequentially_when_index_jumps_ahead<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                    fsm: Arc<MockFsm>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    let mut entries = Vec::new();
                    for i in 1..=3 {
                        entries.push(LogEntry {
                            index: i,
                            term: 1,
                            data: vec![i as u8],
                        });
                    }
                    log_store.append_entries(entries).unwrap();

                    node.advance_last_committed(LogIndex::new(3)).unwrap();

                    let applied = fsm.applied_indices.lock().unwrap();
                    assert_eq!(
                        applied.as_slice(),
                        &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
                    );
                    assert_eq!(node.last_applied(), LogIndex::new(3));
                }

                #[tokio::test]
                async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_follower()
                 {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm.clone(), log_store.clone());
                    check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }

                #[tokio::test]
                async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_candidate()
                 {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm.clone(), log_store.clone());
                    check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }

                #[tokio::test]
                async fn should_apply_multiple_entries_sequentially_when_index_jumps_ahead_as_leader()
                 {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm.clone(), log_store.clone());
                    check_applies_multiple_entries_sequentially_when_index_jumps_ahead(
                        node, fsm, log_store,
                    )
                    .await;
                }
            }

            mod on_stale_index {
                use super::*;

                async fn check_ignores_update_when_index_is_lower_than_current<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    node.advance_last_committed(LogIndex::new(1)).unwrap();

                    node.advance_last_committed(LogIndex::ZERO).unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn should_ignore_update_when_index_is_lower_than_current_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }
            }

            mod on_invalid_index {
                use super::*;

                async fn check_returns_error_when_index_exceeds_last_log_index<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    let result = node.advance_last_committed(LogIndex::new(1));
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("last_log_index"));
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_index_exceeds_last_log_index_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }
            }

            mod on_fsm_failure {
                use super::*;

                #[derive(Debug, Default)]
                struct PoisonFsm;
                impl StateMachine for PoisonFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::ZERO)
                    }

                    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                        Err(FsmError::invariant("Simulated FSM failure"))
                    }

                    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                        Ok(vec![])
                    }

                    fn install_snapshot(
                        &self,
                        _index: LogIndex,
                        _data: &[u8],
                        _trace_id: common::types::trace::TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                async fn check_returns_error_when_state_machine_apply_fails<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![1],
                        }])
                        .unwrap();

                    let result = node.advance_last_committed(LogIndex::new(1));
                    assert!(result.is_err());
                    let err = result.unwrap_err();
                    assert!(
                        matches!(err, NodeError::Protocol(_)),
                        "Expected NodeError::Protocol, got {:?}",
                        err
                    );
                    assert!(err.to_string().contains("Simulated FSM failure"));
                }

                #[tokio::test]
                async fn should_return_error_when_state_machine_apply_fails_as_follower() {
                    let (fsm, log_store) = (Arc::new(PoisonFsm), Arc::new(MemoryStorage::new()));
                    let node = RaftNode::<Follower, PoisonFsm>::try_new(
                        test_identity(1),
                        fsm,
                        log_store.clone(),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    check_returns_error_when_state_machine_apply_fails(node, log_store).await;
                }

                #[tokio::test]
                async fn should_return_error_when_state_machine_apply_fails_as_candidate() {
                    let fsm = Arc::new(PoisonFsm);
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: log_store.clone(),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                    };
                    check_returns_error_when_state_machine_apply_fails(node, log_store).await;
                }

                #[tokio::test]
                async fn should_return_error_when_state_machine_apply_fails_as_leader() {
                    let fsm = Arc::new(PoisonFsm);
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: log_store.clone(),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                    };
                    check_returns_error_when_state_machine_apply_fails(node, log_store).await;
                }
            }
        }

        mod apply_to_state_machine {
            use super::*;

            mod on_fsm_regression {
                use super::*;

                #[derive(Debug, Default)]
                struct RegressionFsm;
                impl StateMachine for RegressionFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::new(100))
                    }

                    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                        Ok(())
                    }

                    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                        Ok(vec![])
                    }

                    fn install_snapshot(
                        &self,
                        _index: LogIndex,
                        _data: &[u8],
                        _trace_id: common::types::trace::TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                async fn check_returns_error_when_fsm_index_is_ahead_of_last_committed<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    let result = node.apply_to_state_machine();
                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("ahead of last_committed")
                    );
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_follower()
                {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::new(100),
                        state: Follower::new(None, Tick::new(0), TickDuration::new(100)),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_candidate()
                 {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_fsm_index_is_ahead_of_last_committed_as_leader() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }
            }

            mod on_physical_corruption {
                use super::*;

                async fn check_returns_error_when_committed_entry_is_missing_from_log_store<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    node.last_committed = LogIndex::new(5);
                    let result = node.apply_to_state_machine();
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("missing from log"));
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_follower()
                 {
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        Arc::new(MockFsm::default()),
                        Arc::new(MemoryStorage::new()),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_candidate()
                 {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(MockFsm::default()),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                    };
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_committed_entry_is_missing_from_log_store_as_leader()
                 {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(MockFsm::default()),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                    };
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
                }
            }
        }

        mod advance_term {
            use super::*;

            mod on_higher_term {
                use super::*;
                const STARTING_TERM: Term = Term::new(5);
                const HIGHER_TERM: Term = Term::new(6);
                const BOOTSTRAP_TERM: Term = Term::new(4);

                async fn check_persists_new_term_and_resets_voted_for<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    node.advance_term(HIGHER_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), HIGHER_TERM);
                    assert_eq!(node.voted_for().unwrap(), None);
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let node = setup_node_as_follower(fsm, log_store);
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // Start at BOOTSTRAP_TERM so setup_node_as_candidate increments to
                    // STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let mut node = setup_node_as_candidate(fsm, log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn should_persist_new_term_and_reset_voted_for_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // Start at BOOTSTRAP_TERM so setup_node_as_leader increments to STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let mut node = setup_node_as_leader(fsm, log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }
            }

            mod on_same_term {
                use super::*;
                const STARTING_TERM: Term = Term::new(5);
                const SAME_TERM: Term = Term::new(5);
                const BOOTSTRAP_TERM: Term = Term::new(4);

                async fn check_preserves_current_term_and_voting_state<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    node.advance_term(SAME_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), STARTING_TERM);
                    assert_eq!(node.voted_for().unwrap(), Some(NodeId::try_new(2).unwrap()));
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::try_new(2).unwrap()))
                        .unwrap();
                    let node = setup_node_as_follower(fsm, log_store);
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_candidate(fsm, log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn should_preserve_current_term_and_voting_state_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_leader(fsm, log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::try_new(2).unwrap()).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }
            }

            mod on_term_regression {
                use super::*;
                const TARGET_TERM: Term = Term::new(10);
                const LOWER_TERM: Term = Term::new(5);

                async fn check_returns_error_when_new_term_is_lower_than_current<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    node.advance_term(TARGET_TERM).unwrap();

                    // Action: Attempt to regress to term 5.
                    let result = node.advance_term(LOWER_TERM);

                    // Verification: Error is returned.
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("regression"));
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn should_return_error_when_new_term_is_lower_than_current_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }
            }

            mod on_storage_failure {
                use common::types::errors::LogStorageError;

                use super::*;

                #[derive(Debug, Default)]
                struct FailingStorage;
                impl LogStorage for FailingStorage {
                    fn current_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::new(1))
                    }

                    fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
                        Ok(None)
                    }

                    fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn last_log_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::ZERO)
                    }

                    fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn read_entry(&self, _: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
                        Ok(None)
                    }

                    fn read_entries(
                        &self,
                        _: LogIndex,
                        _: LogIndex,
                    ) -> Result<Vec<LogEntry>, LogStorageError> {
                        Ok(vec![])
                    }

                    fn save_hard_state(
                        &self,
                        _: Term,
                        _: Option<NodeId>,
                    ) -> Result<(), LogStorageError> {
                        Err(LogStorageError::persistence("Simulated IO Error"))
                    }

                    fn save_last_committed(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn append_entries(&self, _: Vec<LogEntry>) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn truncate_log(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn truncate_log_front(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn save_snapshot_metadata(
                        &self,
                        _: LogIndex,
                        _: Term,
                    ) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
                        Ok(LogIndex::ZERO)
                    }

                    fn last_included_term(&self) -> Result<Term, LogStorageError> {
                        Ok(Term::ZERO)
                    }
                }

                async fn check_propagates_persistence_error_when_storage_fails<
                    R: NodeState,
                    S: StateMachine,
                >(
                    mut node: RaftNode<R, S>,
                ) {
                    let result = node.advance_term(Term::new(2));

                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("Simulated IO Error")
                    );
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_follower() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(FailingStorage);
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        log_store,
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_candidate() {
                    let fsm = Arc::new(MockFsm::default());
                    let _node = RaftNode {
                        identity: test_identity(1),
                        fsm: fsm.clone(),
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(Tick::new(0), TickDuration::new(150)),
                    };
                    check_propagates_persistence_error_when_storage_fails(_node).await;
                }

                #[tokio::test]
                async fn should_propagate_persistence_error_when_storage_fails_as_leader() {
                    let fsm = Arc::new(MockFsm::default());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                    };
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }
            }
        }

        mod persist_vote {
            use super::*;

            mod call {
                use super::*;

                #[test]
                fn should_persist_to_log_store_when_invoked() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node = setup_node_as_follower(fsm, log_store.clone());
                    let candidate_id = NodeId::try_new(2).unwrap();

                    node.persist_vote(candidate_id).unwrap();

                    assert_eq!(node.voted_for().unwrap(), Some(candidate_id));
                    assert_eq!(log_store.voted_for().unwrap(), Some(candidate_id));
                }
            }
        }
    }
}

