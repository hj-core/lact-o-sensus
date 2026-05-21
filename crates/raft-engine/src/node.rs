use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;

use crate::storage::LogStorage;

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

#[derive(Debug)]
pub struct Follower {
    leader_id: Option<NodeId>,
    last_heartbeat: Instant,
}

#[derive(Debug, Default)]
pub struct Candidate {
    votes_received: HashSet<NodeId>,
}

#[derive(Debug, Default)]
pub struct Leader {
    next_index: HashMap<NodeId, LogIndex>,
    match_index: HashMap<NodeId, LogIndex>,
}

pub trait NodeState: Debug {}
impl NodeState for Follower {}
impl NodeState for Candidate {}
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
pub struct RaftNode<S: NodeState> {
    identity: Arc<NodeIdentity>,
    fsm: Arc<dyn StateMachine>,
    log_store: Arc<dyn LogStorage>,

    // --- Volatile State ---
    last_committed: LogIndex,
    last_applied: LogIndex,
    state: S,
}

// --- Implementation: Shared Accessors ---

impl<S: NodeState> RaftNode<S> {
    pub fn cluster_id(&self) -> &ClusterId {
        self.identity.cluster_id()
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    pub fn identity(&self) -> Arc<NodeIdentity> {
        self.identity.clone()
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

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut S {
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

    #[cfg(test)]
    pub(crate) fn log_store(&self) -> &dyn LogStorage {
        self.log_store.as_ref()
    }
}

// --- Implementation: Shared Physical Mutations ---

impl<S: NodeState> RaftNode<S> {
    /// Consumes the current node and returns it in a Follower role.
    /// This is the primary mechanism for demotion and term updates.
    ///
    /// NOTE: This is a pure factory transformation.
    pub fn try_into_follower(
        self,
        term: Term,
        leader_id: Option<NodeId>,
    ) -> Result<RaftNode<Follower>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Follower::new(leader_id),
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
    pub async fn advance_last_committed(&mut self, index: LogIndex) -> Result<(), NodeError> {
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
                "Commit Index Advanced"
            );

            let fsm_span = info_span!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                "fsm_application",
                last_committed = %self.last_committed
            );

            self.apply_to_state_machine().instrument(fsm_span).await?;
        }
        Ok(())
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    async fn apply_to_state_machine(&mut self) -> Result<(), NodeError> {
        // Safety Barrier: Ensure FSM hasn't regressed or moved ahead of log.
        let fsm_last = self.fsm.last_applied_index().map_err(NodeError::from)?;
        if fsm_last > self.last_committed {
            return Err(NodeError::Protocol(format!(
                "FSM index {} is ahead of last_committed {}. Possible log regression.",
                fsm_last, self.last_committed
            )));
        }

        while self.last_applied < self.last_committed {
            let apply_idx = self.last_applied + 1;
            let entry = self.log_store.read_entry(apply_idx)?.ok_or_else(|| {
                NodeError::Protocol(format!(
                    "Committed entry {} missing from log during apply",
                    apply_idx
                ))
            })?;

            if let Err(e) = self.fsm.apply(apply_idx, &entry.data).await {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    index = %apply_idx,
                    error = %e,
                    "State machine failed to apply index. Triggering Halt Mandate."
                );
                return Err(NodeError::from(e));
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
        Arc<dyn StateMachine>,
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
// 4. Role: Follower Behavior
// =============================================================================

impl RaftNode<Follower> {
    pub fn try_new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<dyn StateMachine>,
        log_store: Arc<dyn LogStorage>,
    ) -> Result<Self, NodeError> {
        let last_applied = fsm.last_applied_index().map_err(NodeError::from)?;
        let last_committed = log_store.last_committed().map_err(NodeError::from)?;

        if last_applied > last_committed {
            return Err(NodeError::Protocol(format!(
                "Causal invariant violation: FSM applied index {} is ahead of LogStore committed \
                 index {}",
                last_applied, last_committed
            )));
        }

        Ok(Self {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Follower::new(None),
        })
    }

    /// Following Raft §5.3, reconciles the local log with entries from the
    /// leader.
    pub async fn reconcile_log(
        &mut self,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    ) -> Result<ReconciliationResult, NodeError> {
        let span = info_span!(
            target: ClinicalTarget::RaftReplication.as_str(),
            "log_reconciliation",
            prev_index = %prev_log_index,
            entry_count = entries.len()
        );

        async {
            if !self.verify_log_consistency(prev_log_index, prev_log_term)? {
                return Ok(ReconciliationResult::mismatch(self.last_log_index()?));
            }

            if !self.append_entries_with_reconciliation(entries)? {
                return Ok(ReconciliationResult::mismatch(self.last_log_index()?));
            }

            self.reconcile_last_committed(leader_commit).await?;
            Ok(ReconciliationResult::success(self.last_log_index()?))
        }
        .instrument(span)
        .await
    }

    /// Evaluates if a vote can be granted to a candidate for the given term.
    pub fn attempt_grant_vote(
        &mut self,
        candidate_id: NodeId,
        req_term: Term,
        req_last_log_index: LogIndex,
        req_last_log_term: Term,
    ) -> Result<bool, NodeError> {
        // §5.2, §5.4: Only grant vote if votedFor is null or candidateId,
        // and candidate’s log is at least as up-to-date as receiver’s log.
        let current_term = self.log_store.current_term()?;
        let voted_for = self.log_store.voted_for()?;

        if req_term == current_term
            && (voted_for.is_none() || voted_for == Some(candidate_id))
            && self.is_log_up_to_date(req_last_log_term, req_last_log_index)?
        {
            self.persist_vote(candidate_id)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Follower -> Candidate transition (Triggered by Election Timeout).
    pub fn try_into_candidate(self) -> Result<RaftNode<Candidate>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Candidate::new(),
        };

        let new_term = node.current_term()? + 1;
        node.advance_term(new_term)?;
        let node_id = node.node_id();
        node.persist_vote(node_id)?;
        node.state_mut().add_vote(node_id);

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            term = %new_term,
            "Role Transition: -> Candidate"
        );

        Ok(node)
    }

    // --- Follower Helpers ---

    fn verify_log_consistency(
        &self,
        prev_log_index: LogIndex,
        prev_log_term: Term,
    ) -> Result<bool, NodeError> {
        if prev_log_index == LogIndex::ZERO {
            return Ok(true);
        }

        let last_idx = self.last_log_index()?;
        if prev_log_index > last_idx {
            debug!(
                "Rejecting AppendEntries: prevLogIndex {} is beyond local log length {}",
                prev_log_index, last_idx
            );
            return Ok(false);
        }

        let local_term = self.get_term_at(prev_log_index)?;
        if local_term != prev_log_term {
            debug!(
                "Rejecting AppendEntries: prevLogIndex {} has term mismatch (local {}, remote {})",
                prev_log_index, local_term, prev_log_term
            );
            return Ok(false);
        }

        Ok(true)
    }

    fn append_entries_with_reconciliation(
        &mut self,
        entries: Vec<LogEntry>,
    ) -> Result<bool, NodeError> {
        for entry in &entries {
            let entry_index = LogIndex::new(entry.index);
            let local_term = self.get_term_at(entry_index)?;
            if local_term != Term::ZERO && local_term != Term::new(entry.term) {
                info!(
                    "Log conflict detected at index {}. Truncating log.",
                    entry_index
                );
                self.log_store
                    .truncate_log(entry_index)
                    .map_err(NodeError::from)?;
                break;
            }
        }

        let mut to_append = Vec::new();
        let mut next_expected = self.last_log_index()? + 1;

        for entry in entries {
            let entry_index = LogIndex::new(entry.index);
            if entry_index >= next_expected {
                if entry_index != next_expected {
                    error!(
                        "Non-contiguous log append attempted by Leader. index={}, expected={}",
                        entry_index, next_expected
                    );
                    return Ok(false);
                }
                to_append.push(entry);
                next_expected = next_expected + 1;
            }
        }

        if !to_append.is_empty() {
            self.log_store
                .append_entries(to_append)
                .map_err(NodeError::from)?;
        }

        Ok(true)
    }

    async fn reconcile_last_committed(&mut self, leader_commit: LogIndex) -> Result<(), NodeError> {
        if leader_commit > self.last_committed {
            let last_new_idx = self.last_log_index()?;
            let new_commit = std::cmp::min(leader_commit, last_new_idx);
            self.advance_last_committed(new_commit).await?;
            debug!("Updated last_committed to {}", new_commit);
        }
        Ok(())
    }

    fn is_log_up_to_date(
        &self,
        candidate_last_log_term: Term,
        candidate_last_log_index: LogIndex,
    ) -> Result<bool, NodeError> {
        let local_last_term = self.last_log_term()?;
        let local_last_index = self.last_log_index()?;

        if candidate_last_log_term != local_last_term {
            Ok(candidate_last_log_term > local_last_term)
        } else {
            Ok(candidate_last_log_index >= local_last_index)
        }
    }
}

// =============================================================================
// 5. Role: Candidate Behavior
// =============================================================================

impl RaftNode<Candidate> {
    pub fn try_into_restarted_candidate(self) -> Result<RaftNode<Candidate>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Candidate::new(),
        };

        let new_term = node.current_term()? + 1;
        node.advance_term(new_term)?;
        let node_id = node.node_id();
        node.persist_vote(node_id)?;
        node.state_mut().add_vote(node_id);

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            term = %new_term,
            "Role Transition: -> Candidate (Restarted)"
        );

        Ok(node)
    }

    pub fn try_into_leader(self, peer_ids: Vec<NodeId>) -> Result<RaftNode<Leader>, NodeError> {
        let last_log_index = self.last_log_index()?;
        let term = self.current_term()?;
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Leader::new(peer_ids, last_log_index),
        };

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            term = %term,
            "Role Transition: -> Leader"
        );

        Ok(node)
    }
}

// =============================================================================
// 6. Role: Leader Behavior
// =============================================================================

impl RaftNode<Leader> {
    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, NodeError> {
        let span =
            info_span!(target: ClinicalTarget::RaftReplication.as_str(), "proposal_ingestion");
        let _enter = span.enter();

        let index = self.last_log_index()? + 1;
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

impl Follower {
    pub fn new(leader_id: Option<NodeId>) -> Self {
        Self {
            leader_id,
            last_heartbeat: Instant::now(),
        }
    }

    pub fn leader_id(&self) -> Option<NodeId> {
        self.leader_id
    }

    pub fn last_heartbeat(&self) -> Instant {
        self.last_heartbeat
    }

    pub fn set_leader_id(&mut self, leader_id: Option<NodeId>) {
        self.leader_id = leader_id;
    }

    pub fn reset_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }
}

impl Candidate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_vote(&mut self, peer_id: NodeId) {
        self.votes_received.insert(peer_id);
    }

    pub fn vote_count(&self) -> usize {
        self.votes_received.len()
    }
}

impl Leader {
    pub fn new(peer_ids: Vec<NodeId>, last_log_index: LogIndex) -> Self {
        let mut next_index = HashMap::new();
        let mut match_index = HashMap::new();

        for peer_id in peer_ids {
            next_index.insert(peer_id, last_log_index + 1);
            match_index.insert(peer_id, LogIndex::ZERO);
        }

        Self {
            next_index,
            match_index,
        }
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
}

// =============================================================================
// 8. Behavioral Specification (Tests)
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use common::types::errors::FsmError;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        applied_data: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), FsmError> {
            self.applied_indices.lock().unwrap().push(index);
            self.applied_data.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(id),
        ))
    }

    fn setup_node_as_follower(
        fsm: Arc<MockFsm>,
        log_store: Arc<MemoryStorage>,
    ) -> RaftNode<Follower> {
        RaftNode::try_new(test_identity(1), fsm, log_store).unwrap()
    }

    fn setup_node_as_candidate(
        fsm: Arc<MockFsm>,
        log_store: Arc<MemoryStorage>,
    ) -> RaftNode<Candidate> {
        setup_node_as_follower(fsm, log_store)
            .try_into_candidate()
            .unwrap()
    }

    fn setup_node_as_leader(fsm: Arc<MockFsm>, log_store: Arc<MemoryStorage>) -> RaftNode<Leader> {
        setup_node_as_candidate(fsm, log_store)
            .try_into_leader(vec![])
            .unwrap()
    }

    mod shared {
        use super::*;

        mod state_mut {
            use super::*;

            mod heartbeat_timer {
                use super::*;

                #[test]
                fn reset_heartbeat_updates_timer() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();
                    let initial_time = node.state().last_heartbeat();

                    std::thread::sleep(std::time::Duration::from_millis(1));

                    node.state_mut().reset_heartbeat();

                    assert!(node.state().last_heartbeat() > initial_time);
                }
            }
        }

        mod try_into_follower {
            use super::*;

            mod on_term_update {
                use super::*;

                #[test]
                fn demotion_resets_voting_state_on_new_term() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::new(1)))
                        .unwrap();
                    let node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                            .unwrap();

                    let demoted = node.try_into_follower(Term::new(2), None).unwrap();

                    assert_eq!(demoted.current_term().unwrap(), Term::new(2));
                    assert_eq!(demoted.voted_for().unwrap(), None);
                }

                #[test]
                fn demotion_preserves_vote_on_same_term() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .save_hard_state(Term::new(1), Some(NodeId::new(3)))
                        .unwrap();
                    let node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                            .unwrap();

                    let demoted = node.try_into_follower(Term::new(1), None).unwrap();
                    assert_eq!(demoted.voted_for().unwrap(), Some(NodeId::new(3)));
                }
            }
        }

        mod advance_last_committed {
            use super::*;

            mod on_valid_index {
                use super::*;

                async fn check_persists_to_log_store_when_index_is_valid<S: NodeState>(
                    mut node: RaftNode<S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();

                    node.advance_last_committed(LogIndex::new(1)).await.unwrap();

                    assert_eq!(log_store.last_committed().unwrap(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn persists_to_log_store_when_index_is_valid_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn persists_to_log_store_when_index_is_valid_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                #[tokio::test]
                async fn persists_to_log_store_when_index_is_valid_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store.clone());
                    check_persists_to_log_store_when_index_is_valid(node, log_store).await;
                }

                async fn check_applies_to_fsm_when_index_is_valid<S: NodeState>(
                    mut node: RaftNode<S>,
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

                    node.advance_last_committed(LogIndex::new(1)).await.unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(1));
                    assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
                }

                #[tokio::test]
                async fn applies_to_fsm_when_index_is_valid_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn applies_to_fsm_when_index_is_valid_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                #[tokio::test]
                async fn applies_to_fsm_when_index_is_valid_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm.clone(), log_store.clone());
                    check_applies_to_fsm_when_index_is_valid(node, fsm, log_store).await;
                }

                async fn check_applies_multiple_entries_sequentially_when_index_jumps_ahead<
                    S: NodeState,
                >(
                    mut node: RaftNode<S>,
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

                    node.advance_last_committed(LogIndex::new(3)).await.unwrap();

                    let applied = fsm.applied_indices.lock().unwrap();
                    assert_eq!(
                        applied.as_slice(),
                        &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
                    );
                    assert_eq!(node.last_applied(), LogIndex::new(3));
                }

                #[tokio::test]
                async fn applies_multiple_entries_sequentially_when_index_jumps_ahead_as_follower()
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
                async fn applies_multiple_entries_sequentially_when_index_jumps_ahead_as_candidate()
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
                async fn applies_multiple_entries_sequentially_when_index_jumps_ahead_as_leader() {
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

                async fn check_ignores_update_when_index_is_lower_than_current<S: NodeState>(
                    mut node: RaftNode<S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    node.advance_last_committed(LogIndex::new(1)).await.unwrap();

                    node.advance_last_committed(LogIndex::ZERO).await.unwrap();

                    assert_eq!(node.last_committed(), LogIndex::new(1));
                }

                #[tokio::test]
                async fn ignores_update_when_index_is_lower_than_current_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn ignores_update_when_index_is_lower_than_current_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }

                #[tokio::test]
                async fn ignores_update_when_index_is_lower_than_current_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_leader(fsm, log_store.clone());
                    check_ignores_update_when_index_is_lower_than_current(node, log_store).await;
                }
            }

            mod on_invalid_index {
                use super::*;

                async fn check_returns_error_when_index_exceeds_last_log_index<S: NodeState>(
                    mut node: RaftNode<S>,
                ) {
                    let result = node.advance_last_committed(LogIndex::new(1)).await;
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("last_log_index"));
                }

                #[tokio::test]
                async fn returns_error_when_index_exceeds_last_log_index_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_index_exceeds_last_log_index_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store);
                    check_returns_error_when_index_exceeds_last_log_index(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_index_exceeds_last_log_index_as_leader() {
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
                #[async_trait]
                impl StateMachine for PoisonFsm {
                    fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
                        Ok(LogIndex::ZERO)
                    }

                    async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
                        Err(FsmError::invariant("Simulated FSM failure"))
                    }
                }

                async fn check_returns_error_when_state_machine_apply_fails<S: NodeState>(
                    mut node: RaftNode<S>,
                    log_store: Arc<MemoryStorage>,
                ) {
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![1],
                        }])
                        .unwrap();

                    let result = node.advance_last_committed(LogIndex::new(1)).await;
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
                async fn returns_error_when_state_machine_apply_fails_as_follower() {
                    let (_fsm, log_store) = (Arc::new(PoisonFsm), Arc::new(MemoryStorage::new()));
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(PoisonFsm),
                        log_store.clone(),
                    )
                    .unwrap();
                    check_returns_error_when_state_machine_apply_fails(node, log_store).await;
                }

                #[tokio::test]
                async fn returns_error_when_state_machine_apply_fails_as_candidate() {
                    let fsm = Arc::new(PoisonFsm);
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: log_store.clone(),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(),
                    };
                    check_returns_error_when_state_machine_apply_fails(node, log_store).await;
                }

                #[tokio::test]
                async fn returns_error_when_state_machine_apply_fails_as_leader() {
                    let fsm = Arc::new(PoisonFsm);
                    let log_store = Arc::new(MemoryStorage::new());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: log_store.clone(),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO),
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
                #[async_trait]
                impl StateMachine for RegressionFsm {
                    fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
                        Ok(LogIndex::new(100))
                    }

                    async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
                        Ok(())
                    }
                }

                async fn check_returns_error_when_fsm_index_is_ahead_of_last_committed<
                    S: NodeState,
                >(
                    mut node: RaftNode<S>,
                ) {
                    let result = node.apply_to_state_machine().await;
                    assert!(result.is_err());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("ahead of last_committed")
                    );
                }

                #[tokio::test]
                async fn returns_error_when_fsm_index_is_ahead_of_last_committed_as_follower() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::new(100),
                        state: Follower::new(None),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_fsm_index_is_ahead_of_last_committed_as_candidate() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_fsm_index_is_ahead_of_last_committed_as_leader() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(RegressionFsm),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO),
                    };
                    check_returns_error_when_fsm_index_is_ahead_of_last_committed(node).await;
                }
            }

            mod on_physical_corruption {
                use super::*;

                async fn check_returns_error_when_committed_entry_is_missing_from_log_store<
                    S: NodeState,
                >(
                    mut node: RaftNode<S>,
                ) {
                    node.last_committed = LogIndex::new(5);
                    let result = node.apply_to_state_machine().await;
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("missing from log"));
                }

                #[tokio::test]
                async fn returns_error_when_committed_entry_is_missing_from_log_store_as_follower()
                {
                    let node = RaftNode::<Follower>::try_new(
                        test_identity(1),
                        Arc::new(MockFsm::default()),
                        Arc::new(MemoryStorage::new()),
                    )
                    .unwrap();
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_committed_entry_is_missing_from_log_store_as_candidate()
                {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(MockFsm::default()),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(),
                    };
                    check_returns_error_when_committed_entry_is_missing_from_log_store(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_committed_entry_is_missing_from_log_store_as_leader() {
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm: Arc::new(MockFsm::default()),
                        log_store: Arc::new(MemoryStorage::new()),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO),
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

                async fn check_persists_new_term_and_resets_voted_for<S: NodeState>(
                    mut node: RaftNode<S>,
                ) {
                    node.advance_term(HIGHER_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), HIGHER_TERM);
                    assert_eq!(node.voted_for().unwrap(), None);
                }

                #[tokio::test]
                async fn persists_new_term_and_resets_voted_for_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::new(2)))
                        .unwrap();
                    let node = setup_node_as_follower(fsm, log_store);
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn persists_new_term_and_resets_voted_for_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // Start at BOOTSTRAP_TERM so setup_node_as_candidate increments to
                    // STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::new(2)))
                        .unwrap();
                    let mut node = setup_node_as_candidate(fsm, log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::new(2)).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }

                #[tokio::test]
                async fn persists_new_term_and_resets_voted_for_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // Start at BOOTSTRAP_TERM so setup_node_as_leader increments to STARTING_TERM.
                    log_store
                        .save_hard_state(BOOTSTRAP_TERM, Some(NodeId::new(2)))
                        .unwrap();
                    let mut node = setup_node_as_leader(fsm, log_store);
                    // Manually inject a vote for someone else to verify clearing.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::new(2)).unwrap();
                    check_persists_new_term_and_resets_voted_for(node).await;
                }
            }

            mod on_same_term {
                use super::*;
                const STARTING_TERM: Term = Term::new(5);
                const SAME_TERM: Term = Term::new(5);
                const BOOTSTRAP_TERM: Term = Term::new(4);

                async fn check_preserves_current_term_and_voting_state<S: NodeState>(
                    mut node: RaftNode<S>,
                ) {
                    node.advance_term(SAME_TERM).unwrap();
                    assert_eq!(node.current_term().unwrap(), STARTING_TERM);
                    assert_eq!(node.voted_for().unwrap(), Some(NodeId::new(2)));
                }

                #[tokio::test]
                async fn preserves_current_term_and_voting_state_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    log_store
                        .save_hard_state(STARTING_TERM, Some(NodeId::new(2)))
                        .unwrap();
                    let node = setup_node_as_follower(fsm, log_store);
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn preserves_current_term_and_voting_state_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_candidate(fsm, log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::new(2)).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }

                #[tokio::test]
                async fn preserves_current_term_and_voting_state_as_leader() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    // setup results in STARTING_TERM.
                    log_store.save_hard_state(BOOTSTRAP_TERM, None).unwrap();
                    let mut node = setup_node_as_leader(fsm, log_store);
                    // Ensure voted_for is node 2 as expected by check.
                    node.advance_term(STARTING_TERM).unwrap();
                    node.persist_vote(NodeId::new(2)).unwrap();
                    check_preserves_current_term_and_voting_state(node).await;
                }
            }

            mod on_term_regression {
                use super::*;
                const TARGET_TERM: Term = Term::new(10);
                const LOWER_TERM: Term = Term::new(5);

                async fn check_returns_error_when_new_term_is_lower_than_current<S: NodeState>(
                    mut node: RaftNode<S>,
                ) {
                    node.advance_term(TARGET_TERM).unwrap();

                    // Action: Attempt to regress to term 5.
                    let result = node.advance_term(LOWER_TERM);

                    // Verification: Error is returned.
                    assert!(result.is_err());
                    assert!(result.unwrap_err().to_string().contains("regression"));
                }

                #[tokio::test]
                async fn returns_error_when_new_term_is_lower_than_current_as_follower() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_follower(fsm, log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_new_term_is_lower_than_current_as_candidate() {
                    let (fsm, log_store) =
                        (Arc::new(MockFsm::default()), Arc::new(MemoryStorage::new()));
                    let node = setup_node_as_candidate(fsm, log_store);
                    check_returns_error_when_new_term_is_lower_than_current(node).await;
                }

                #[tokio::test]
                async fn returns_error_when_new_term_is_lower_than_current_as_leader() {
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
                }

                async fn check_propagates_persistence_error_when_storage_fails<S: NodeState>(
                    mut node: RaftNode<S>,
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
                async fn propagates_persistence_error_when_storage_fails_as_follower() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(FailingStorage);
                    let node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }

                #[tokio::test]
                async fn propagates_persistence_error_when_storage_fails_as_candidate() {
                    let fsm = Arc::new(MockFsm::default());
                    let _node = RaftNode {
                        identity: test_identity(1),
                        fsm: fsm.clone(),
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Candidate::new(),
                    };
                    check_propagates_persistence_error_when_storage_fails(_node).await;
                }

                #[tokio::test]
                async fn propagates_persistence_error_when_storage_fails_as_leader() {
                    let fsm = Arc::new(MockFsm::default());
                    let node = RaftNode {
                        identity: test_identity(1),
                        fsm,
                        log_store: Arc::new(FailingStorage),
                        last_committed: LogIndex::ZERO,
                        last_applied: LogIndex::ZERO,
                        state: Leader::new(vec![], LogIndex::ZERO),
                    };
                    check_propagates_persistence_error_when_storage_fails(node).await;
                }
            }
        }

        mod persist_vote {
            use super::*;

            #[test]
            fn persists_vote_to_log_store() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_follower(fsm, log_store.clone());
                let candidate_id = NodeId::new(2);

                node.persist_vote(candidate_id).unwrap();

                assert_eq!(node.voted_for().unwrap(), Some(candidate_id));
                assert_eq!(log_store.voted_for().unwrap(), Some(candidate_id));
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
                fn recovers_state_from_log_store_on_initialization() {
                    let fsm = Arc::new(MockFsm::default());
                    let dir = tempfile::tempdir().unwrap();

                    {
                        let db = sled::open(dir.path()).unwrap();
                        let log_store = SledStorage::new(db).unwrap();
                        log_store
                            .save_hard_state(Term::new(7), Some(NodeId::new(2)))
                            .unwrap();
                    }

                    {
                        let db = sled::open(dir.path()).unwrap();
                        let log_store = Arc::new(SledStorage::new(db).unwrap());
                        let node = RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store)
                            .unwrap();

                        assert_eq!(node.current_term().unwrap(), Term::new(7));
                        assert_eq!(node.voted_for().unwrap(), Some(NodeId::new(2)));
                    }
                }
            }

            mod on_causal_divergence {
                use super::*;

                #[derive(Debug, Default)]
                struct AheadFsm;
                #[async_trait]
                impl StateMachine for AheadFsm {
                    fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
                        Ok(LogIndex::new(100))
                    }

                    async fn apply(&self, _: LogIndex, _: &[u8]) -> Result<(), FsmError> {
                        Ok(())
                    }
                }

                #[test]
                fn returns_error_when_fsm_is_ahead_of_log_store() {
                    let fsm = Arc::new(AheadFsm);
                    let log_store = Arc::new(MemoryStorage::new());

                    let result = RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store);

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
                async fn rejects_inconsistent_prev_index() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();
                    let result = node
                        .reconcile_log(LogIndex::new(1), Term::new(1), vec![], LogIndex::ZERO)
                        .await
                        .unwrap();
                    assert!(!result.success);
                }
            }

            mod on_conflicting_entries {
                use super::*;

                #[tokio::test]
                async fn detects_conflicts_and_truncates() {
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
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
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
                        .await
                        .unwrap();

                    assert!(result.success);
                    assert_eq!(result.last_index, LogIndex::new(2));
                    assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
                }

                #[tokio::test]
                async fn truncates_conflicting_suffix() {
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
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
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
                        .await
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
                async fn is_idempotent_for_duplicate_entries() {
                    let fsm = Arc::new(MockFsm::default());
                    let entry = LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![1],
                    };
                    let log_store = MemoryStorage::new();
                    log_store.append_entries(vec![entry.clone()]).unwrap();
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                            .unwrap();

                    let result = node
                        .reconcile_log(LogIndex::ZERO, Term::ZERO, vec![entry], LogIndex::ZERO)
                        .await
                        .unwrap();

                    assert!(result.success);
                    assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
                }
            }

            mod on_non_contiguous_append {
                use super::*;

                #[tokio::test]
                async fn rejects_non_contiguous_append() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();
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
                        .await
                        .unwrap();

                    assert!(!result.success);
                    assert_eq!(node.last_log_index().unwrap(), LogIndex::ZERO);
                }

                #[tokio::test]
                async fn rejects_gap_between_prev_index_and_entries() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                            .unwrap();

                    let entry3 = LogEntry {
                        index: 3,
                        term: 1,
                        data: vec![],
                    };

                    let result = node
                        .reconcile_log(LogIndex::new(1), Term::new(1), vec![entry3], LogIndex::ZERO)
                        .await
                        .unwrap();

                    assert!(!result.success);
                }
            }

            mod on_commit_advancement {
                use super::*;

                #[tokio::test]
                async fn caps_last_committed_at_local_log_length() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = MemoryStorage::new();
                    log_store
                        .append_entries(vec![LogEntry {
                            index: 1,
                            term: 1,
                            data: vec![],
                        }])
                        .unwrap();
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                            .unwrap();

                    // Scenario: Leader has commit_index 10, but our log only reaches 2 after
                    // append.
                    let entry2 = LogEntry {
                        index: 2,
                        term: 1,
                        data: vec![],
                    };

                    node.reconcile_log(
                        LogIndex::new(1),
                        Term::new(1),
                        vec![entry2],
                        LogIndex::new(10),
                    )
                    .await
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
                fn grant_vote_respects_voting_state() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let mut node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();
                    node.advance_term(Term::new(1)).unwrap();
                    node.persist_vote(NodeId::new(3)).unwrap();

                    let granted = node
                        .attempt_grant_vote(
                            NodeId::new(2),
                            Term::new(1),
                            LogIndex::ZERO,
                            Term::ZERO,
                        )
                        .unwrap();
                    assert!(!granted);

                    let granted = node
                        .attempt_grant_vote(
                            NodeId::new(3),
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

                fn setup_node_with_log(last_idx: u64, last_term: u64) -> RaftNode<Follower> {
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
                    RaftNode::<Follower>::try_new(test_identity(1), fsm, Arc::new(log_store))
                        .unwrap()
                }

                #[test]
                fn rejects_stale_last_term() {
                    let node = setup_node_with_log(5, 2);
                    assert!(
                        !node
                            .is_log_up_to_date(Term::new(1), LogIndex::new(10))
                            .unwrap()
                    );
                }

                #[test]
                fn accepts_higher_last_term() {
                    let node = setup_node_with_log(5, 2);
                    assert!(
                        node.is_log_up_to_date(Term::new(3), LogIndex::new(1))
                            .unwrap()
                    );
                }

                #[test]
                fn handles_equal_terms_with_index_check() {
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
                fn promotion_preserves_invariants() {
                    let fsm = Arc::new(MockFsm::default());
                    let log_store = Arc::new(MemoryStorage::new());
                    let node =
                        RaftNode::<Follower>::try_new(test_identity(1), fsm, log_store).unwrap();

                    let candidate = node.try_into_candidate().unwrap();

                    assert_eq!(candidate.current_term().unwrap(), Term::new(1));
                    assert_eq!(candidate.voted_for().unwrap(), Some(NodeId::new(1)));
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
            fn increments_term_and_votes_for_self() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let node = setup_node_as_candidate(fsm, log_store);
                let initial_term = node.current_term().unwrap();

                let restarted = node.try_into_restarted_candidate().unwrap();

                assert_eq!(restarted.current_term().unwrap(), initial_term + 1);
                assert_eq!(restarted.voted_for().unwrap(), Some(restarted.node_id()));
                assert_eq!(restarted.state().vote_count(), 1);
            }
        }

        mod on_election_victory {
            use super::*;

            #[test]
            fn initializes_leader_state_with_next_index_at_end_of_log() {
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
                let peer_ids = vec![NodeId::new(2), NodeId::new(3)];

                let leader = node.try_into_leader(peer_ids.clone()).unwrap();

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
            fn add_vote_is_idempotent_per_peer() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_candidate(fsm, log_store);

                node.state_mut().add_vote(NodeId::new(2));
                node.state_mut().add_vote(NodeId::new(2)); // Duplicate

                assert_eq!(node.state().vote_count(), 2); // Self (from setup) + Node 2
            }
        }
    }

    mod leader {
        use super::*;

        mod propose {
            use super::*;

            #[test]
            fn increments_log_length_and_uses_current_term() {
                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(MemoryStorage::new());
                let mut node = setup_node_as_leader(fsm, log_store);
                let current_term = node.current_term().unwrap();

                let index = node.propose(vec![42]).unwrap();

                assert_eq!(index, LogIndex::new(1));
                assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
                let entry = node.read_entries(index, index).unwrap().remove(0);
                assert_eq!(entry.term, current_term.value());
                assert_eq!(entry.data, vec![42]);
            }

            #[test]
            fn returns_error_when_storage_fails() {
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
                }

                let fsm = Arc::new(MockFsm::default());
                let log_store = Arc::new(FailingAppendStorage);
                let mut node = RaftNode {
                    identity: test_identity(1),
                    fsm,
                    log_store,
                    last_committed: LogIndex::ZERO,
                    last_applied: LogIndex::ZERO,
                    state: Leader::new(vec![], LogIndex::ZERO),
                };

                let result = node.propose(vec![1]);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("Append Failure"));
            }
        }
    }
}
