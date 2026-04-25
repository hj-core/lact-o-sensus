use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use tokio::sync::Notify;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::fsm::StateMachine;

// =============================================================================
// 1. Public Snapshots & Types
// =============================================================================

/// Snapshot of the node's consensus progress, used for reactive observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusProgress {
    pub term: Term,
    pub commit_index: LogIndex,
    /// Volatile counter representing the logical epoch of the node's consistent
    /// state.
    ///
    /// INCREMENT POLICY: This counter MUST be incremented exactly once upon the
    /// successful completion of any:
    /// 1. Physical Mutation: Advancing the Term, appending to the Log, or
    ///    updating the Commit Index.
    /// 2. Logical Transition: Changing the node's Role (e.g., Candidate ->
    ///    Leader), even if Term/Index are unchanged.
    ///
    /// This ensures that observers on the reactive channel are notified of
    /// every significant consensus event and never miss a state change.
    pub signal_counter: u64,
}

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

// =============================================================================
// 2. Role Markers (Type-State Engine)
// =============================================================================

#[derive(Debug)]
pub struct Follower {
    leader_id: Option<NodeId>,
    last_heartbeat: Instant,
    heartbeat_signal: Arc<Notify>,
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

pub trait NodeState: std::fmt::Debug {}
impl NodeState for Follower {}
impl NodeState for Candidate {}
impl NodeState for Leader {}

// =============================================================================
// 3. Shared Shared Container (RaftNode)
// =============================================================================

/// Container for Raft state that is shared across all roles or must persist.
///
/// Represents the "Physical Node" layer, managing log storage, term
/// persistence, and the commitment boundary.
///
/// SILENT STATE MACHINE: This struct is a pure data mutator. It does NOT own
/// signaling channels or perform I/O. Signaling is the responsibility of the
/// high-level orchestrator shell.
#[derive(Debug)]
pub struct RaftNode<S: NodeState> {
    node_id: NodeId,
    fsm: Arc<dyn StateMachine>,

    // --- Persistent State ---
    current_term: Term,
    voted_for: Option<NodeId>,
    log: Vec<LogEntry>,

    // --- Volatile State ---
    commit_index: LogIndex,
    last_applied: LogIndex,
    /// Volatile counter representing the logical epoch of the node's consistent
    /// state.
    ///
    /// Following the INCREMENT POLICY, this counter is updated on every
    /// physical mutation or logical role transition to trigger reactive
    /// observers.
    signal_counter: u64,
    state: S,
}

// --- Implementation: Shared Accessors ---

impl<S: NodeState> RaftNode<S> {
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn current_term(&self) -> Term {
        self.current_term
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }

    pub fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    pub fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    pub fn signal_counter(&self) -> u64 {
        self.signal_counter
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    pub(crate) fn log(&self) -> &[LogEntry] {
        &self.log
    }

    #[cfg(test)]
    pub(crate) fn log_mut(&mut self) -> &mut Vec<LogEntry> {
        &mut self.log
    }

    // --- Log Queries ---

    pub fn last_log_index(&self) -> LogIndex {
        self.log
            .last()
            .map(|e| LogIndex::new(e.index))
            .unwrap_or(LogIndex::ZERO)
    }

    pub fn last_log_term(&self) -> Term {
        self.log
            .last()
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO)
    }

    pub fn get_term_at(&self, index: LogIndex) -> Term {
        if index == LogIndex::ZERO {
            return Term::ZERO;
        }
        self.log
            .get((index.value() - 1) as usize)
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO)
    }
}

// --- Implementation: Shared Physical Mutations ---

impl<S: NodeState> RaftNode<S> {
    /// Consumes the current node and returns it in a Follower role.
    /// This is the primary mechanism for demotion and term updates.
    ///
    /// NOTE: This is a pure factory transformation.
    pub fn into_follower(self, term: Term, leader_id: Option<NodeId>) -> RaftNode<Follower> {
        let (
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
        ) = self.into_parts();

        let mut node = RaftNode {
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
            state: Follower::new(leader_id),
        };

        // Transition to next term if higher (§5.1)
        // INCREMENT POLICY: We increment for the term update AND the role change.
        node.set_term(term);
        node.increment_signal();
        node
    }

    /// Updates the commit index and triggers the application of entries to the
    /// FSM.
    pub async fn set_commit_index(&mut self, index: LogIndex) {
        if index < self.commit_index {
            debug!(
                "Ignoring stale commit_index update: {} < current {}",
                index, self.commit_index
            );
            return;
        }

        let last_idx = self.last_log_index();
        if index > last_idx {
            panic!(
                "CRITICAL: Protocol violation. Attempted to commit index {} but last_log_index is \
                 {}",
                index, last_idx
            );
        }

        if index > self.commit_index {
            self.commit_index = index;
            self.apply_to_state_machine().await;
            self.increment_signal();
        }
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    async fn apply_to_state_machine(&mut self) {
        while self.last_applied < self.commit_index {
            let apply_idx = self.last_applied + 1;
            let entry = self
                .log
                .get((apply_idx.value() - 1) as usize)
                .expect("Halt Mandate: Committed entry missing from log during apply");

            if let Err(e) = self.fsm.apply(apply_idx, &entry.data).await {
                error!(
                    "CRITICAL: State machine failed to apply index {}: {}. Triggering Halt \
                     Mandate.",
                    apply_idx, e
                );
                // TODO: Classify errors. Transient errors (IO, Timeout) should trigger
                // exponential backoff retry. Deterministic errors (Logic, Invariant
                // Violation) MUST panic to prevent cluster-wide state divergence.
                panic!("Halt Mandate: FSM application failed.");
            }

            self.last_applied = apply_idx;
        }
    }

    /// Updates the current term and resets voting state if the term increased.
    pub(crate) fn set_term(&mut self, term: Term) {
        if term < self.current_term {
            panic!(
                "CRITICAL: Term regression detected! current={} new={}",
                self.current_term, term
            );
        }
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
            self.increment_signal();
        }
    }

    pub fn vote_for(&mut self, candidate_id: NodeId) {
        self.voted_for = Some(candidate_id);
    }

    pub(crate) fn increment_signal(&mut self) {
        self.signal_counter += 1;
    }

    fn into_parts(
        self,
    ) -> (
        NodeId,
        Arc<dyn StateMachine>,
        Term,
        Option<NodeId>,
        Vec<LogEntry>,
        LogIndex,
        LogIndex,
        u64,
    ) {
        (
            self.node_id,
            self.fsm,
            self.current_term,
            self.voted_for,
            self.log,
            self.commit_index,
            self.last_applied,
            self.signal_counter,
        )
    }
}

// =============================================================================
// 4. Role: Follower Behavior
// =============================================================================

impl RaftNode<Follower> {
    pub fn new(node_id: NodeId, fsm: Arc<dyn StateMachine>) -> Self {
        Self {
            node_id,
            fsm,
            current_term: Term::ZERO,
            voted_for: None,
            log: Vec::new(),
            commit_index: LogIndex::ZERO,
            last_applied: LogIndex::ZERO,
            signal_counter: 0,
            state: Follower::new(None),
        }
    }

    /// Following Raft §5.3, reconciles the local log with entries from the
    /// leader.
    pub async fn reconcile_log(
        &mut self,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    ) -> ReconciliationResult {
        let old_last_idx = self.last_log_index();
        let old_last_term = self.last_log_term();

        if !self.verify_log_consistency(prev_log_index, prev_log_term) {
            return ReconciliationResult::mismatch(self.last_log_index());
        }

        if !self.append_entries_with_reconciliation(entries) {
            return ReconciliationResult::mismatch(self.last_log_index());
        }

        if self.last_log_index() != old_last_idx || self.last_log_term() != old_last_term {
            self.increment_signal();
        }

        self.advance_commit_index(leader_commit).await;
        ReconciliationResult::success(self.last_log_index())
    }

    /// Follower -> Candidate transition (Triggered by Election Timeout).
    pub fn into_candidate(self) -> RaftNode<Candidate> {
        let (
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
        ) = self.into_parts();

        let mut node = RaftNode {
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
            state: Candidate::new(),
        };

        // INCREMENT POLICY: We increment for the term update AND the role change.
        node.set_term(current_term + 1);
        node.vote_for(node_id);
        node.state_mut().add_vote(node_id);
        node.increment_signal();
        node
    }

    /// Evaluates if a vote can be granted to a candidate for the given term.
    pub fn attempt_grant_vote(
        &mut self,
        candidate_id: NodeId,
        req_term: Term,
        req_last_log_index: LogIndex,
        req_last_log_term: Term,
    ) -> bool {
        // §5.2, §5.4: Only grant vote if votedFor is null or candidateId,
        // and candidate’s log is at least as up-to-date as receiver’s log.
        if req_term == self.current_term
            && (self.voted_for.is_none() || self.voted_for == Some(candidate_id))
        {
            if self.is_log_up_to_date(req_last_log_term, req_last_log_index) {
                self.vote_for(candidate_id);
                return true;
            }
        }
        false
    }

    // --- Follower Helpers ---

    fn verify_log_consistency(&self, prev_log_index: LogIndex, prev_log_term: Term) -> bool {
        if prev_log_index == LogIndex::ZERO {
            return true;
        }

        if prev_log_index > self.last_log_index() {
            debug!(
                "Rejecting AppendEntries: prevLogIndex {} is beyond local log length {}",
                prev_log_index,
                self.last_log_index()
            );
            return false;
        }

        let local_term = self.get_term_at(prev_log_index);
        if local_term != prev_log_term {
            debug!(
                "Rejecting AppendEntries: prevLogIndex {} has term mismatch (local {}, remote {})",
                prev_log_index, local_term, prev_log_term
            );
            return false;
        }

        true
    }

    fn append_entries_with_reconciliation(&mut self, entries: Vec<LogEntry>) -> bool {
        for entry in &entries {
            let entry_index = LogIndex::new(entry.index);
            let local_term = self.get_term_at(entry_index);
            if local_term != Term::ZERO && local_term != Term::new(entry.term) {
                info!(
                    "Log conflict detected at index {}. Truncating log.",
                    entry_index
                );
                let truncate_at = (entry.index - 1) as usize;
                self.log.truncate(truncate_at);
                break;
            }
        }

        for entry in entries {
            let entry_index = LogIndex::new(entry.index);
            let last_idx = self.last_log_index();
            if entry_index > last_idx {
                if entry_index != last_idx + 1 {
                    error!(
                        "CRITICAL: Non-contiguous log append attempted by Leader. index={}, \
                         last={}",
                        entry_index, last_idx
                    );
                    return false;
                }
                self.log.push(entry);
            }
        }

        true
    }

    async fn advance_commit_index(&mut self, leader_commit: LogIndex) {
        if leader_commit > self.commit_index {
            let last_new_idx = self.last_log_index();
            let new_commit = std::cmp::min(leader_commit, last_new_idx);
            self.set_commit_index(new_commit).await;
            debug!("Updated commit_index to {}", new_commit);
        }
    }

    fn is_log_up_to_date(
        &self,
        candidate_last_log_term: Term,
        candidate_last_log_index: LogIndex,
    ) -> bool {
        let local_last_term = self.last_log_term();
        let local_last_index = self.last_log_index();

        if candidate_last_log_term != local_last_term {
            candidate_last_log_term > local_last_term
        } else {
            candidate_last_log_index >= local_last_index
        }
    }
}

// =============================================================================
// 5. Role: Candidate Behavior
// =============================================================================

impl RaftNode<Candidate> {
    pub fn into_restarted_candidate(self) -> RaftNode<Candidate> {
        let (
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
        ) = self.into_parts();

        let mut node = RaftNode {
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
            state: Candidate::new(),
        };

        // INCREMENT POLICY: We increment for the term update AND the role change.
        node.set_term(current_term + 1);
        node.vote_for(node_id);
        node.state_mut().add_vote(node_id);
        node.increment_signal();
        node
    }

    pub fn into_leader(self, peer_ids: Vec<NodeId>) -> RaftNode<Leader> {
        let last_log_index = self.last_log_index();
        let (
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
        ) = self.into_parts();

        let mut node = RaftNode {
            node_id,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            signal_counter,
            state: Leader::new(peer_ids, last_log_index),
        };
        node.increment_signal();
        node
    }
}

// =============================================================================
// 6. Role: Leader Behavior
// =============================================================================

impl RaftNode<Leader> {
    pub fn propose(&mut self, command: Vec<u8>) -> LogIndex {
        let index = self.last_log_index() + 1;
        let term = self.current_term;
        let entry = LogEntry::new(index, term, command);
        self.log.push(entry);
        self.increment_signal();
        index
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
            heartbeat_signal: Arc::new(Notify::new()),
        }
    }

    pub fn leader_id(&self) -> Option<NodeId> {
        self.leader_id
    }

    pub fn last_heartbeat(&self) -> Instant {
        self.last_heartbeat
    }

    pub fn heartbeat_signal(&self) -> &Arc<Notify> {
        &self.heartbeat_signal
    }

    pub fn set_leader_id(&mut self, leader_id: Option<NodeId>) {
        self.leader_id = leader_id;
    }

    pub fn reset_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
        self.heartbeat_signal.notify_one();
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
            match_index.insert(peer_id, LogIndex::new(0));
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
    use tonic::Status;

    use super::*;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        applied_data: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl StateMachine for MockFsm {
        async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Status> {
            self.applied_indices.lock().unwrap().push(index);
            self.applied_data.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    mod shared_primitives {
        use super::*;

        #[test]
        #[should_panic(expected = "CRITICAL: Term regression detected")]
        fn set_term_panics_on_regression() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            node.set_term(Term::new(10));
            node.set_term(Term::new(5));
        }

        #[tokio::test]
        async fn set_commit_index_applies_to_fsm() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm.clone());
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![1]));

            node.set_commit_index(LogIndex::new(1)).await;

            assert_eq!(node.commit_index(), LogIndex::new(1));
            assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn set_commit_index_ignores_stale_index() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));
            node.set_commit_index(LogIndex::new(1)).await;

            let signal_before = node.signal_counter();
            node.set_commit_index(LogIndex::ZERO).await;

            assert_eq!(node.commit_index(), LogIndex::new(1));
            assert_eq!(node.signal_counter(), signal_before);
        }

        #[tokio::test]
        async fn set_commit_index_applies_multiple_entries_sequentially() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm.clone());
            for i in 1..=3 {
                node.log_mut()
                    .push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![i as u8]));
            }

            // Jump from index 0 to 3
            node.set_commit_index(LogIndex::new(3)).await;

            let applied = fsm.applied_indices.lock().unwrap();
            assert_eq!(
                applied.as_slice(),
                &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
            );
            assert_eq!(node.last_applied(), LogIndex::new(3));
        }

        #[tokio::test]
        #[should_panic(expected = "Committed entry missing from log")]
        async fn apply_to_state_machine_panics_on_physical_log_corruption() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            // Manually advance commit index beyond log length WITHOUT the set_commit_index
            // guard
            node.commit_index = LogIndex::new(5);
            node.apply_to_state_machine().await;
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL: Protocol violation")]
        async fn set_commit_index_panics_on_boundary_violation() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            // Log is empty (max index 0), trying to commit 1
            node.set_commit_index(LogIndex::new(1)).await;
        }
    }

    mod follower_ops {
        use super::*;

        fn setup_node() -> RaftNode<Follower> {
            let fsm = Arc::new(MockFsm::default());
            RaftNode::<Follower>::new(NodeId::new(1), fsm)
        }

        #[tokio::test]
        async fn reconcile_log_rejects_inconsistent_prev_index() {
            let mut node = setup_node();
            // Node has empty log. Leader claims prev is 1.
            let result = node
                .reconcile_log(LogIndex::new(1), Term::new(1), vec![], LogIndex::ZERO)
                .await;
            assert!(!result.success);
        }

        #[tokio::test]
        async fn reconcile_log_detects_conflicts_and_truncates() {
            let mut node = setup_node();
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![1]));
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(2), Term::new(1), vec![2]));

            // Leader sends entry for index 2 with Term 2.
            let new_entry = LogEntry::new(LogIndex::new(2), Term::new(2), vec![3]);
            let result = node
                .reconcile_log(
                    LogIndex::new(1),
                    Term::new(1),
                    vec![new_entry],
                    LogIndex::ZERO,
                )
                .await;

            assert!(result.success);
            assert_eq!(result.last_index, LogIndex::new(2));
            assert_eq!(node.get_term_at(LogIndex::new(2)), Term::new(2));
            assert_eq!(node.log().len(), 2);
        }

        #[tokio::test]
        async fn reconcile_log_truncates_conflicting_suffix() {
            let mut node = setup_node();
            // Log: [ (1, T1), (2, T1), (3, T1) ]
            for i in 1..=3 {
                node.log_mut()
                    .push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
            }

            // Leader sends conflict at index 2, but NO entry for index 3.
            // Result should be: [ (1, T1), (2, T2) ]
            let new_entry = LogEntry::new(LogIndex::new(2), Term::new(2), vec![]);
            let result = node
                .reconcile_log(
                    LogIndex::new(1),
                    Term::new(1),
                    vec![new_entry],
                    LogIndex::ZERO,
                )
                .await;

            assert!(result.success);
            assert_eq!(result.last_index, LogIndex::new(2));
            assert_eq!(node.log().len(), 2);
            assert_eq!(node.get_term_at(LogIndex::new(2)), Term::new(2));
        }

        #[tokio::test]
        async fn reconcile_log_is_idempotent_for_duplicate_entries() {
            let mut node = setup_node();
            let entry = LogEntry::new(LogIndex::new(1), Term::new(1), vec![1]);
            node.log_mut().push(entry.clone());

            let signal_before = node.signal_counter();
            let result = node
                .reconcile_log(LogIndex::ZERO, Term::ZERO, vec![entry], LogIndex::ZERO)
                .await;

            assert!(result.success);
            assert_eq!(node.log().len(), 1);
            // No physical change -> No signal increment
            assert_eq!(node.signal_counter(), signal_before);
        }

        #[tokio::test]
        async fn reconcile_log_rejects_non_contiguous_append() {
            let mut node = setup_node();
            // Log empty (index 0). Leader sends entries for 2 and 3.
            let entry2 = LogEntry::new(LogIndex::new(2), Term::new(1), vec![]);
            let entry3 = LogEntry::new(LogIndex::new(3), Term::new(1), vec![]);

            let result = node
                .reconcile_log(
                    LogIndex::new(1),
                    Term::new(1),
                    vec![entry2, entry3],
                    LogIndex::ZERO,
                )
                .await;

            assert!(!result.success);
            assert_eq!(node.last_log_index(), LogIndex::ZERO);
        }

        #[test]
        fn grant_vote_respects_voting_state() {
            let mut node = setup_node();
            node.set_term(Term::new(1));
            node.vote_for(NodeId::new(3)); // Already voted for someone else

            let granted =
                node.attempt_grant_vote(NodeId::new(2), Term::new(1), LogIndex::ZERO, Term::ZERO);
            assert!(!granted);

            // Can still grant to the SAME candidate
            let granted =
                node.attempt_grant_vote(NodeId::new(3), Term::new(1), LogIndex::ZERO, Term::ZERO);
            assert!(granted);
        }
    }

    mod up_to_date_rule {
        use super::*;

        fn setup_node_with_log(last_idx: u64, last_term: u64) -> RaftNode<Follower> {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            for i in 1..last_idx {
                node.log_mut()
                    .push(LogEntry::new(LogIndex::new(i), Term::new(1), vec![]));
            }
            if last_idx > 0 {
                node.log_mut().push(LogEntry::new(
                    LogIndex::new(last_idx),
                    Term::new(last_term),
                    vec![],
                ));
            }
            node
        }

        #[test]
        fn rejects_stale_last_term() {
            let node = setup_node_with_log(5, 2);
            // Candidate has Term 1, Node has Term 2
            assert!(!node.is_log_up_to_date(Term::new(1), LogIndex::new(10)));
        }

        #[test]
        fn accepts_higher_last_term() {
            let node = setup_node_with_log(5, 2);
            // Candidate has Term 3, Node has Term 2
            assert!(node.is_log_up_to_date(Term::new(3), LogIndex::new(1)));
        }

        #[test]
        fn handles_equal_terms_with_index_check() {
            let node = setup_node_with_log(5, 2);
            // Same Term (2), but Candidate index 4 < Node index 5
            assert!(!node.is_log_up_to_date(Term::new(2), LogIndex::new(4)));
            // Same Term (2), Candidate index 5 == Node index 5
            assert!(node.is_log_up_to_date(Term::new(2), LogIndex::new(5)));
            // Same Term (2), Candidate index 6 > Node index 5
            assert!(node.is_log_up_to_date(Term::new(2), LogIndex::new(6)));
        }
    }

    mod heartbeat_invariants {
        use futures::FutureExt;

        use super::*;

        #[test]
        fn reset_heartbeat_updates_timer_and_notifies() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            let initial_time = node.state().last_heartbeat();
            let notify = node.state().heartbeat_signal().clone();

            // Small sleep to ensure Instant changes
            std::thread::sleep(std::time::Duration::from_millis(1));

            node.state_mut().reset_heartbeat();

            assert!(node.state().last_heartbeat() > initial_time);
            // In a real test we'd check notification, but it's internal to Notify.
            // We can check it doesn't block.
            assert!(notify.notified().now_or_never().is_some());
        }
    }

    mod transitions {
        use super::*;

        #[test]
        fn candidate_transition_preserves_invariants() {
            let fsm = Arc::new(MockFsm::default());
            let node = RaftNode::<Follower>::new(NodeId::new(1), fsm);

            let candidate = node.into_candidate();

            assert_eq!(candidate.current_term(), Term::new(1));
            assert_eq!(candidate.voted_for(), Some(NodeId::new(1)));
            assert_eq!(candidate.state().vote_count(), 1);
        }

        #[test]
        fn leader_transition_initializes_indices_correctly() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            let peer_id = NodeId::new(2);
            let leader = node.into_candidate().into_leader(vec![peer_id]);

            // Raft §5.3: initialize nextIndex to leader last log index + 1
            let next_idx = leader.state().next_index().get(&peer_id).unwrap();
            assert_eq!(next_idx.value(), 2);

            // Raft §5.3: initialize matchIndex to 0
            let match_idx = leader.state().match_index().get(&peer_id).unwrap();
            assert_eq!(match_idx.value(), 0);
        }

        #[test]
        fn demotion_resets_voting_state_on_new_term() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            node.set_term(Term::new(1));
            node.vote_for(NodeId::new(1));

            let demoted = node.into_follower(Term::new(2), None);

            assert_eq!(demoted.current_term(), Term::new(2));
            assert_eq!(demoted.voted_for(), None);
        }

        #[test]
        fn demotion_preserves_vote_on_same_term() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm);
            node.set_term(Term::new(1));
            node.vote_for(NodeId::new(3));

            let demoted = node.into_follower(Term::new(1), None);
            assert_eq!(demoted.voted_for(), Some(NodeId::new(3)));
        }

        #[test]
        fn into_restarted_candidate_increments_term_and_signal() {
            let fsm = Arc::new(MockFsm::default());
            let node = RaftNode::<Follower>::new(NodeId::new(1), fsm).into_candidate(); // Term 1

            let signal_before = node.signal_counter();
            let restarted = node.into_restarted_candidate(); // Term 2

            assert_eq!(restarted.current_term(), Term::new(2));
            assert_eq!(restarted.voted_for(), Some(NodeId::new(1)));
            assert!(restarted.signal_counter() > signal_before);
        }
    }

    mod leader_ops {
        use super::*;

        #[test]
        fn propose_increases_epoch() {
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(NodeId::new(1), fsm)
                .into_candidate()
                .into_leader(vec![]);

            let initial_signal = node.signal_counter();
            node.propose(vec![1]);

            assert!(node.signal_counter() > initial_signal);
            assert_eq!(node.last_log_index(), LogIndex::new(1));
        }
    }
}
