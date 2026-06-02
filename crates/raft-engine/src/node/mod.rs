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

use std::collections::HashMap;
use std::collections::HashSet;
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

pub use candidate::*;
pub use follower::*;

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
#[derive(Debug)]
pub struct Leader {
    next_index: HashMap<NodeId, LogIndex>,
    match_index: HashMap<NodeId, LogIndex>,
    last_heartbeat: Tick,

    /// The epoch of the latest heartbeat round initiated (§8).
    current_read_epoch: u64,
    /// The highest epoch acknowledged by a majority (§8).
    confirmed_read_epoch: u64,
    /// Peers who have acknowledged the current_read_epoch.
    heartbeat_acks: HashSet<NodeId>,
}

pub trait NodeState: Debug {}
impl NodeState for Leader {}

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
// 6. Role: Leader Behavior
// =============================================================================

impl<S: StateMachine> RaftNode<Leader, S> {
    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    #[instrument(
        name = "proposal_ingestion",
        target = "raft::replication",
        skip_all,
        fields(command_len = command.len())
    )]
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, NodeError> {
        let index = (self.last_log_index()? + 1)?;
        let entry = LogEntry::new(index, self.current_term()?, command);
        self.log_store
            .append_entries(vec![entry])
            .map_err(NodeError::from)?;
        Ok(index)
    }
}

// =============================================================================
// 7. Role Marker Boilerplate
// =============================================================================

impl Leader {
    pub fn new(
        peer_ids: Vec<NodeId>,
        last_log_index: LogIndex,
        last_heartbeat: Tick,
    ) -> Result<Self, NodeError> {
        let mut next_index = HashMap::new();
        let mut match_index = HashMap::new();

        for peer_id in peer_ids {
            next_index.insert(peer_id, (last_log_index + 1)?);
            match_index.insert(peer_id, LogIndex::ZERO);
        }

        Ok(Self {
            next_index,
            match_index,
            last_heartbeat,
            current_read_epoch: 0,
            confirmed_read_epoch: 0,
            heartbeat_acks: HashSet::new(),
        })
    }

    pub fn last_heartbeat(&self) -> Tick {
        self.last_heartbeat
    }

    pub fn reset_heartbeat(&mut self, current_tick: Tick) {
        self.last_heartbeat = current_tick;
    }

    pub fn next_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.next_index
    }

    pub fn next_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.next_index
    }

    pub fn match_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.match_index
    }

    pub fn match_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.match_index
    }

    pub fn current_read_epoch(&self) -> u64 {
        self.current_read_epoch
    }

    pub fn confirmed_read_epoch(&self) -> u64 {
        self.confirmed_read_epoch
    }

    pub fn evaluate_tick(&self, now: Tick, threshold: TickDuration) -> TickAction {
        if now - self.last_heartbeat >= threshold {
            TickAction::SendHeartbeat
        } else {
            TickAction::None
        }
    }

    /// Prepares a new read probe epoch (§8).
    ///
    /// If the current epoch has already reached quorum (or is otherwise
    /// finished), increments the epoch to ensure the next round-trip proves
    /// authority *after* this call. Returns the target epoch to wait for.
    pub fn prepare_read_probe(&mut self, self_id: NodeId) -> u64 {
        if self.confirmed_read_epoch == self.current_read_epoch {
            self.current_read_epoch += 1;
            self.heartbeat_acks.clear();
            self.heartbeat_acks.insert(self_id);
        }
        self.current_read_epoch
    }

    /// Records an acknowledgment for the current heartbeat epoch (§8).
    ///
    /// If a majority is reached, advances the `confirmed_read_epoch`.
    pub fn acknowledge_heartbeat(&mut self, peer_id: NodeId, quorum_size: usize) {
        self.heartbeat_acks.insert(peer_id);
        if self.heartbeat_acks.len() >= quorum_size {
            self.confirmed_read_epoch = self.current_read_epoch;
        }
    }
}

// =============================================================================
// 8. Behavioral Specification (Tests)
// =============================================================================

#[cfg(test)]
mod tests {
    use std::result::Result;
    use std::sync::Mutex;

    use common::types::errors::FsmError;
    use common::types::trace::TraceId;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        applied_data: Mutex<Vec<Vec<u8>>>,
    }

    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Self::Error> {
            self.applied_indices
                .lock()
                .expect("Clinical Invariant: Mutex must be lockable")
                .push(index);
            self.applied_data
                .lock()
                .expect("Clinical Invariant: Mutex must be lockable")
                .push(data.to_vec());
            Ok(())
        }

        fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        fn install_snapshot(
            &self,
            _last_included_index: LogIndex,
            _data: &[u8],
            _trace_id: TraceId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(id).unwrap(),
        ))
    }

    fn setup_node_as_follower(
        fsm: Arc<MockFsm>,
        log_store: Arc<MemoryStorage>,
    ) -> RaftNode<Follower, MockFsm> {
        RaftNode::try_new(
            test_identity(1),
            fsm,
            log_store,
            Tick::new(0),
            TickDuration::new(100),
        )
        .unwrap()
    }

    fn setup_node_as_candidate(
        fsm: Arc<MockFsm>,
        log_store: Arc<MemoryStorage>,
    ) -> RaftNode<Candidate, MockFsm> {
        setup_node_as_follower(fsm, log_store)
            .try_into_candidate(Tick::new(0), TickDuration::new(150))
            .unwrap()
    }

    fn setup_node_as_leader(
        fsm: Arc<MockFsm>,
        log_store: Arc<MemoryStorage>,
    ) -> RaftNode<Leader, MockFsm> {
        setup_node_as_candidate(fsm, log_store)
            .try_into_leader(vec![], Tick::new(0))
            .unwrap()
    }

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
                        _trace_id: TraceId,
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
                        _trace_id: TraceId,
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

    mod follower {
        use super::*;

        mod try_new {
            use super::*;
            use crate::storage::SledStorage;

            mod on_causal_recovery {
                use super::*;

                #[test]
                fn should_recover_state_from_log_store_on_initialization() {
                    let fsm = Arc::new(MockFsm::default());
                    let dir = tempfile::tempdir().unwrap();

                    {
                        let db = sled::open(dir.path()).unwrap();
                        let log_store = SledStorage::new(db).unwrap();
                        log_store
                            .save_hard_state(Term::new(7), Some(NodeId::try_new(2).unwrap()))
                            .unwrap();
                    }

                    {
                        let db = sled::open(dir.path()).unwrap();
                        let log_store = Arc::new(SledStorage::new(db).unwrap());
                        let node = RaftNode::<Follower, MockFsm>::try_new(
                            test_identity(1),
                            fsm,
                            log_store,
                            Tick::new(0),
                            TickDuration::new(100),
                        )
                        .unwrap();

                        assert_eq!(node.current_term().unwrap(), Term::new(7));
                        assert_eq!(node.voted_for().unwrap(), Some(NodeId::try_new(2).unwrap()));
                    }
                }
            }

            mod on_causal_divergence {
                use super::*;

                #[derive(Debug, Default)]
                struct AheadFsm;
                impl StateMachine for AheadFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::new(100))
                    }

                    fn apply(&self, _: LogIndex, _: &[u8]) -> Result<(), Self::Error> {
                        Ok(())
                    }

                    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                        Ok(vec![])
                    }

                    fn install_snapshot(
                        &self,
                        _: LogIndex,
                        _: &[u8],
                        _: TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                #[test]
                fn should_return_error_when_fsm_is_ahead_of_log_store() {
                    let fsm = Arc::new(AheadFsm);
                    let log_store = Arc::new(MemoryStorage::new());

                    let result = RaftNode::<Follower, AheadFsm>::try_new(
                        test_identity(1),
                        fsm,
                        log_store,
                        Tick::new(0),
                        TickDuration::new(100),
                    );

                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("Causal invariant violation")
                    );
                }
            }
        }

        mod reconcile_log {
            use super::*;

            mod on_consistency_mismatch {
                use super::*;

                #[tokio::test]
                async fn should_reject_append_entries_when_prev_index_is_inconsistent() {
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
                    let result = node
                        .reconcile_log(LogIndex::new(1), Term::new(1), vec![], LogIndex::ZERO)
                        .unwrap();
                    assert!(!result.success);
                }
            }

            mod on_conflicting_entries {
                use super::*;

                #[tokio::test]
                async fn should_detect_conflicts_and_truncate_log() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .append_entries(vec![
                            LogEntry {
                                index: 1,
                                term: 1,
                                data: vec![1],
                            },
                            LogEntry {
                                index: 2,
                                term: 1,
                                data: vec![2],
                            },
                        ])
                        .unwrap();
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let new_entry = LogEntry {
                        index: 2,
                        term: 2,
                        data: vec![3],
                    };
                    let result = node
                        .reconcile_log(
                            LogIndex::new(1),
                            Term::new(1),
                            vec![new_entry],
                            LogIndex::ZERO,
                        )
                        .unwrap();

                    assert!(result.success);
                    assert_eq!(result.last_index, LogIndex::new(2));
                    assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
                }

                #[tokio::test]
                async fn should_truncate_conflicting_suffix() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    let mut entries = Vec::new();
                    for i in 1..=3 {
                        entries.push(LogEntry {
                            index: i,
                            term: 1,
                            data: vec![],
                        });
                    }
                    log_store.append_entries(entries).unwrap();
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let new_entry = LogEntry {
                        index: 2,
                        term: 2,
                        data: vec![],
                    };
                    let result = node
                        .reconcile_log(
                            LogIndex::new(1),
                            Term::new(1),
                            vec![new_entry],
                            LogIndex::ZERO,
                        )
                        .unwrap();

                    assert!(result.success);
                    assert_eq!(result.last_index, LogIndex::new(2));
                    assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
                    assert_eq!(node.last_log_index().unwrap(), LogIndex::new(2));
                }
            }

            mod on_duplicate_entries {
                use super::*;

                #[tokio::test]
                async fn should_be_idempotent_when_duplicate_entries_received() {
                    let fsm = Arc::new(MockFsm::default());
                    let entry = LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![1],
                    };
                    let log_store = MemoryStorage::new();
                    log_store.append_entries(vec![entry.clone()]).unwrap();
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let result = node
                        .reconcile_log(LogIndex::ZERO, Term::ZERO, vec![entry], LogIndex::ZERO)
                        .unwrap();

                    assert!(result.success);
                    assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
                }
            }

            mod on_non_contiguous_append {
                use super::*;

                #[tokio::test]
                async fn should_reject_non_contiguous_append() {
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
                    let entry2 = LogEntry {
                        index: 2,
                        term: 1,
                        data: vec![],
                    };
                    let entry3 = LogEntry {
                        index: 3,
                        term: 1,
                        data: vec![],
                    };

                    let result = node
                        .reconcile_log(
                            LogIndex::new(1),
                            Term::new(1),
                            vec![entry2, entry3],
                            LogIndex::ZERO,
                        )
                        .unwrap();

                    assert!(!result.success);
                    assert_eq!(node.last_log_index().unwrap(), LogIndex::ZERO);
                }

                #[tokio::test]
                async fn should_reject_append_when_gap_exists_between_prev_index_and_entries() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let entry3 = LogEntry {
                        index: 3,
                        term: 1,
                        data: vec![],
                    };

                    let result = node
                        .reconcile_log(LogIndex::new(1), Term::new(1), vec![entry3], LogIndex::ZERO)
                        .unwrap();

                    assert!(!result.success);
                }
            }

            mod on_commit_advancement {
                use super::*;

                #[tokio::test]
                async fn should_cap_last_committed_at_local_log_length() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    let mut node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    // Scenario: Leader has commit_index 10, but our log only reaches 2 after
                    // append.
                    let entry2 = LogEntry {
                        index: 2,
                        term: 1,
                        data: vec![],
                    };

                    let _result = node
                        .reconcile_log(
                            LogIndex::new(1),
                            Term::new(1),
                            vec![entry2],
                            LogIndex::new(10),
                        )
                        .unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(2));
                }
            }
        }

        mod attempt_grant_vote {
            use super::*;

            mod on_existing_vote {
                use super::*;

                #[test]
                fn should_respect_voting_state_when_attempting_to_grant_vote() {
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
                    node.advance_term(Term::new(1)).unwrap();
                    node.persist_vote(NodeId::try_new(3).unwrap()).unwrap();

                    let granted = node
                        .attempt_grant_vote(
                            NodeId::try_new(2).unwrap(),
                            Term::new(1),
                            LogIndex::ZERO,
                            Term::ZERO,
                        )
                        .unwrap();
                    assert!(!granted);

                    let granted = node
                        .attempt_grant_vote(
                            NodeId::try_new(3).unwrap(),
                            Term::new(1),
                            LogIndex::ZERO,
                            Term::ZERO,
                        )
                        .unwrap();
                    assert!(granted);
                }
            }

            mod on_log_up_to_date_check {
                use super::*;

                fn setup_node_with_log(
                    last_idx: u64,
                    last_term: u64,
                ) -> RaftNode<Follower, MockFsm> {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    let mut entries = Vec::new();
                    for i in 1..last_idx {
                        entries.push(LogEntry {
                            index: i,
                            term: 1,
                            data: vec![],
                        });
                    }
                    if last_idx > 0 {
                        entries.push(LogEntry {
                            index: last_idx,
                            term: last_term,
                            data: vec![],
                        });
                    }
                    log_store.append_entries(entries).unwrap();
                    RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        Arc::new(log_store),
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap()
                }

                #[test]
                fn should_reject_vote_when_last_term_is_stale() {
                    let node = setup_node_with_log(5, 2);
                    assert!(
                        !node
                            .is_log_up_to_date(Term::new(1), LogIndex::new(10))
                            .unwrap()
                    );
                }

                #[test]
                fn should_accept_vote_when_last_term_is_higher() {
                    let node = setup_node_with_log(5, 2);
                    assert!(
                        node.is_log_up_to_date(Term::new(3), LogIndex::new(1))
                            .unwrap()
                    );
                }

                #[test]
                fn should_handle_index_check_when_terms_are_equal() {
                    let node = setup_node_with_log(5, 2);
                    assert!(
                        !node
                            .is_log_up_to_date(Term::new(2), LogIndex::new(4))
                            .unwrap()
                    );
                    assert!(
                        node.is_log_up_to_date(Term::new(2), LogIndex::new(5))
                            .unwrap()
                    );
                    assert!(
                        node.is_log_up_to_date(Term::new(2), LogIndex::new(6))
                            .unwrap()
                    );
                }
            }
        }

        mod try_into_candidate {
            use super::*;

            mod on_election_timeout {
                use super::*;

                #[test]
                fn should_preserve_invariants_when_promoting_to_candidate() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = RaftNode::<Follower, MockFsm>::try_new(
                        test_identity(1),
                        fsm,
                        log_store,
                        Tick::new(0),
                        TickDuration::new(100),
                    )
                    .unwrap();

                    let candidate = node
                        .try_into_candidate(Tick::new(0), TickDuration::new(150))
                        .unwrap();

                    assert_eq!(candidate.current_term().unwrap(), Term::new(1));
                    assert_eq!(
                        candidate.voted_for().unwrap(),
                        Some(NodeId::try_new(1).unwrap())
                    );
                    assert_eq!(candidate.state().vote_count(), 1);
                }
            }
        }
    }

    mod candidate {
        use super::*;

        mod on_election_restart {
            use super::*;

            #[test]
            fn should_increment_term_and_vote_for_self_on_election_restart() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let node = setup_node_as_candidate(fsm, log_store);
                let initial_term = node.current_term().unwrap();

                let restarted = node
                    .try_into_restarted_candidate(Tick::new(0), TickDuration::new(150))
                    .unwrap();

                assert_eq!(
                    restarted.current_term().unwrap(),
                    (initial_term + 1).unwrap()
                );
                assert_eq!(restarted.voted_for().unwrap(), Some(restarted.node_id()));
                assert_eq!(restarted.state().vote_count(), 1);
            }
        }

        mod on_election_victory {
            use super::*;

            #[test]
            fn should_initialize_leader_state_with_next_index_at_end_of_log() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                // Append some entries to the log
                log_store
                    .append_entries(vec![
                        LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        },
                        LogEntry {
                            index: 2,
                            term: 1,
                            data: vec![],
                        },
                    ])
                    .unwrap();

                let node = setup_node_as_candidate(fsm, log_store);
                let peer_ids = vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()];

                let leader = node
                    .try_into_leader(peer_ids.clone(), Tick::new(0))
                    .unwrap();

                assert_eq!(leader.last_log_index().unwrap(), LogIndex::new(2));
                for peer_id in peer_ids {
                    assert_eq!(
                        *leader.state().next_index().get(&peer_id).unwrap(),
                        LogIndex::new(3)
                    );
                    assert_eq!(
                        *leader.state().match_index().get(&peer_id).unwrap(),
                        LogIndex::ZERO
                    );
                }
            }
        }

        mod vote_counting {
            use super::*;

            #[test]
            fn should_be_idempotent_when_adding_vote_per_peer() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_candidate(fsm, log_store);

                node.state_mut().add_vote(NodeId::try_new(2).unwrap());
                node.state_mut().add_vote(NodeId::try_new(2).unwrap()); // Duplicate

                assert_eq!(node.state().vote_count(), 2); // Self (from setup) + Node 2
            }
        }
    }

    mod leader {
        use super::*;

        mod propose {
            use super::*;

            #[test]
            fn should_increment_log_length_and_use_current_term() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_leader(fsm, log_store);
                let current_term = node.current_term().unwrap();

                let index = node.propose(vec![42]).unwrap();

                assert_eq!(index, LogIndex::new(1));
                assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
                let entry = node.read_entries(index, index).unwrap().remove(0);
                assert_eq!(Term::new(entry.term), current_term);
                assert_eq!(entry.data, vec![42]);
            }

            #[test]
            fn should_return_error_when_storage_fails() {
                use common::types::errors::LogStorageError;

                #[derive(Debug, Default)]
                struct FailingAppendStorage;
                impl LogStorage for FailingAppendStorage {
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
                        Ok(())
                    }

                    fn save_last_committed(&self, _: LogIndex) -> Result<(), LogStorageError> {
                        Ok(())
                    }

                    fn append_entries(&self, _: Vec<LogEntry>) -> Result<(), LogStorageError> {
                        Err(LogStorageError::persistence("Simulated Append Failure"))
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

                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(FailingAppendStorage);
                let mut node = RaftNode {
                    identity: test_identity(1),
                    fsm,
                    log_store,
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
                };

                let result = node.propose(vec![1]);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("Append Failure"));
            }
        }

        mod heartbeat_epochs {
            use super::*;

            #[test]
            fn should_advance_epoch_when_quorum_is_reached() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_candidate(fsm, log_store)
                    .try_into_leader(
                        vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                        Tick::new(0),
                    )
                    .unwrap();
                let self_id = node.node_id();

                // 1. Initial state
                assert_eq!(node.state().current_read_epoch(), 0);
                assert_eq!(node.state().confirmed_read_epoch(), 0);

                // 2. Start round 1 (prepare_read_probe increments to 1)
                let target = node.state_mut().prepare_read_probe(self_id);
                assert_eq!(target, 1);

                // 3. Acknowledge from peer 2 (1 peer + self = 2/3 quorum)
                node.state_mut()
                    .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
                assert_eq!(node.state().confirmed_read_epoch(), 1);

                // 4. Start round 2 (increments to 2)
                let target2 = node.state_mut().prepare_read_probe(self_id);
                assert_eq!(target2, 2);
                assert_eq!(node.state().confirmed_read_epoch(), 1);

                // 5. Reach quorum for round 2
                node.state_mut()
                    .acknowledge_heartbeat(NodeId::try_new(3).unwrap(), 2);
                assert_eq!(node.state().confirmed_read_epoch(), 2);
            }
        }
    }
}
