use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::fsm::StateMachine;
use crate::identity::NodeIdentity;

#[derive(Error, Debug)]
pub enum RaftError {
    #[error("Node is in an unrecoverable state due to a failed transition")]
    Poisoned,

    #[error("Node is not the leader and cannot propose mutations")]
    NotLeader,
}

// --- Type-State Markers (Role-Specific Volatile State) ---

#[derive(Debug)]
pub struct Follower {
    /// The ID of the current leader (if known).
    leader_id: Option<NodeId>,

    /// The instant when the last valid heartbeat was received.
    last_heartbeat: Instant,

    /// Signal used to notify the election timer that a heartbeat was received.
    heartbeat_signal: Arc<Notify>,
}

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

    /// Updates the leader ID.
    pub fn set_leader_id(&mut self, leader_id: Option<NodeId>) {
        self.leader_id = leader_id;
    }

    /// Resets the heartbeat timer and signals the election timer.
    pub fn reset_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
        self.heartbeat_signal.notify_one();
    }
}

impl Default for Follower {
    fn default() -> Self {
        Self::new(None)
    }
}

#[derive(Debug, Default)]
pub struct Candidate {
    /// Set of peer IDs who have granted their vote to this candidate in the
    /// current term.
    votes_received: HashSet<NodeId>,
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

#[derive(Debug, Default)]
pub struct Leader {
    /// For each server, index of the next log entry to send to that server
    /// (initialized to leader last log index + 1).
    next_index: HashMap<NodeId, LogIndex>,

    /// For each server, index of highest log entry known to be replicated on
    /// server (initialized to 0, increases monotonically).
    match_index: HashMap<NodeId, LogIndex>,
}

impl Leader {
    /// Initializes leader-specific volatile state.
    ///
    /// nextIndex is initialized to (lastLogIndex + 1), and matchIndex is
    /// initialized to 0 for all peers.
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

pub trait NodeState: std::fmt::Debug {}
impl NodeState for Follower {}
impl NodeState for Candidate {}
impl NodeState for Leader {}

// --- Generic Node Struct (Persistent & Volatile State) ---

/// Container for Raft state that is shared across all roles or must persist.
#[derive(Debug)]
pub struct RaftNode<S: NodeState> {
    /// Verified identity of the node (ADR 004).
    identity: Arc<NodeIdentity>,

    /// The application state machine (ADR 007).
    fsm: Arc<dyn StateMachine>,

    // --- Persistent State ---
    /// Persistent term across role transitions.
    current_term: Term,

    /// CandidateId that received vote in current term (or None if none).
    voted_for: Option<NodeId>,

    /// The replicated log (1-based indexing used logically).
    log: Vec<LogEntry>,

    // --- Volatile State (Shared) ---
    /// Index of highest log entry known to be committed.
    commit_index: LogIndex,

    /// Index of highest log entry applied to state machine.
    last_applied: LogIndex,

    /// Signal triggered whenever commit_index increases.
    commit_signal: Arc<Notify>,

    /// Role-specific volatile state marker.
    state: S,
}

impl<S: NodeState> RaftNode<S> {
    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    /// Returns a reference to the node's identity Arc.
    pub fn identity_arc(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    pub fn current_term(&self) -> Term {
        self.current_term
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }

    pub fn log(&self) -> &Vec<LogEntry> {
        &self.log
    }

    pub fn log_mut(&mut self) -> &mut Vec<LogEntry> {
        &mut self.log
    }

    pub fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    pub fn commit_signal(&self) -> &Arc<Notify> {
        &self.commit_signal
    }

    /// Updates the commit index and triggers the application of entries to the
    /// FSM.
    ///
    /// Adheres to the monotonicity requirement: stale updates are ignored.
    /// This method acts as a high-level orchestrator, delegating the physical
    /// application to `apply_to_state_machine`.
    ///
    /// # Panics
    /// Panics if the new index exceeds the current log boundaries.
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
            self.commit_signal.notify_waiters();
        }
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    ///
    /// # Panics
    /// Panics (Halt Mandate) if an entry is missing from the log or if the
    /// State Machine returns a terminal error.
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
                panic!("Halt Mandate: FSM application failed.");
            }

            self.last_applied = apply_idx;
        }
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

    /// Returns the index of the last entry in the log (0 if empty).
    pub fn last_log_index(&self) -> LogIndex {
        self.log
            .last()
            .map(|e| LogIndex::new(e.index))
            .unwrap_or(LogIndex::ZERO)
    }

    /// Returns the term of the last entry in the log (0 if empty).
    pub fn last_log_term(&self) -> Term {
        self.log
            .last()
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO)
    }

    /// Returns the term of the log entry at the given index.
    /// Returns 0 if index is 0 or out of bounds.
    pub fn get_term_at(&self, index: LogIndex) -> Term {
        if index == LogIndex::ZERO {
            return Term::ZERO;
        }
        // Assuming contiguous log entries starting at index 1.
        self.log
            .get((index.value() - 1) as usize)
            .map(|e| Term::new(e.term))
            .unwrap_or(Term::ZERO)
    }

    /// Verifies if a candidate's log is at least as up-to-date as the local
    /// log.
    ///
    /// Raft §5.4:
    /// If the logs have last entries with different terms, then the log with
    /// the later term is more up-to-date. If the logs end with the same
    /// term, then whichever log is longer is more up-to-date.
    pub fn is_log_up_to_date(
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

    /// Internal helper to update term and reset vote.
    fn set_term(&mut self, term: Term) {
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
        }
    }

    /// Records a vote for a candidate in the current term.
    pub fn vote_for(&mut self, candidate_id: NodeId) {
        self.voted_for = Some(candidate_id);
        // TODO: Phase 6 - fsync to sled
    }

    /// Decomposes the node into its transition-invariant components.
    fn into_parts(
        self,
    ) -> (
        Arc<NodeIdentity>,
        Arc<dyn StateMachine>,
        Term,
        Option<NodeId>,
        Vec<LogEntry>,
        LogIndex,
        LogIndex,
        Arc<Notify>,
    ) {
        (
            self.identity,
            self.fsm,
            self.current_term,
            self.voted_for,
            self.log,
            self.commit_index,
            self.last_applied,
            self.commit_signal,
        )
    }
}

impl RaftNode<Follower> {
    pub fn new(identity: Arc<NodeIdentity>, fsm: Arc<dyn StateMachine>) -> Self {
        Self {
            identity,
            fsm,
            current_term: Term::ZERO,
            voted_for: None,
            log: Vec::new(),
            commit_index: LogIndex::ZERO,
            last_applied: LogIndex::ZERO,
            commit_signal: Arc::new(Notify::new()),
            state: Follower::default(),
        }
    }

    /// Following Raft §5.3, reconciles the local log with entries from the
    /// leader.
    ///
    /// This is a high-level orchestrator for Follower log updates.
    /// Returns (success, last_log_index).
    pub async fn reconcile_log(
        &mut self,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    ) -> (bool, LogIndex) {
        // 2. Reply false if log doesn’t contain an entry at prevLogIndex whose term
        //    matches prevLogTerm (§5.3)
        if !self.verify_log_consistency(prev_log_index, prev_log_term) {
            return (false, self.last_log_index());
        }

        // 3 & 4. Handle conflicts, truncation, and appending.
        if !self.append_entries_with_reconciliation(entries) {
            return (false, self.last_log_index());
        }

        // 5. If leaderCommit > commitIndex, set commitIndex = min(leaderCommit, index
        //    of last new entry)
        self.advance_commit_index(leader_commit).await;

        (true, self.last_log_index())
    }

    /// Raft §5.3 (Step 2): Verifies that the log contains an entry at
    /// `prev_log_index` with `prev_log_term`.
    fn verify_log_consistency(&self, prev_log_index: LogIndex, prev_log_term: Term) -> bool {
        if prev_log_index == LogIndex::ZERO {
            return true;
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

    /// Raft §5.3 (Steps 3 & 4): Resolves log conflicts and appends new entries.
    fn append_entries_with_reconciliation(&mut self, entries: Vec<LogEntry>) -> bool {
        // 3. If an existing entry conflicts with a new one (same index but different
        //    terms), delete the existing entry and all that follow it (§5.3)
        for entry in &entries {
            let entry_index = LogIndex::new(entry.index);
            let local_term = self.get_term_at(entry_index);
            if local_term != Term::ZERO && local_term != Term::new(entry.term) {
                info!(
                    "Log conflict detected at index {}. Truncating log.",
                    entry_index
                );
                let truncate_at = (entry.index - 1) as usize;
                self.log_mut().truncate(truncate_at);
                // TODO: Phase 6 - fsync truncation to sled
                break;
            }
        }

        // 4. Append any new entries not already in the log
        for entry in entries {
            let entry_index = LogIndex::new(entry.index);
            let last_idx = self.last_log_index();
            if entry_index > last_idx {
                if entry_index != last_idx + 1 {
                    // This should be caught by the prevLogIndex check, but we are defensive.
                    error!(
                        "CRITICAL: Non-contiguous log append attempted. index={}, last={}",
                        entry_index, last_idx
                    );
                    return false;
                }
                self.log_mut().push(entry);
                // TODO: Phase 6 - fsync append to sled
            }
        }

        true
    }

    /// Raft §5.3 (Step 5): Advances the commit index.
    async fn advance_commit_index(&mut self, leader_commit: LogIndex) {
        if leader_commit > self.commit_index() {
            let last_new_idx = self.last_log_index();
            let new_commit = std::cmp::min(leader_commit, last_new_idx);
            self.set_commit_index(new_commit).await;
            debug!("Updated commit_index to {}", new_commit);
        }
    }

    /// Follower -> Candidate transition (Triggered by Election Timeout).
    pub fn into_candidate(self) -> RaftNode<Candidate> {
        let mut state = Candidate::new();
        state.add_vote(self.identity.node_id());

        RaftNode {
            identity: self.identity.clone(),
            fsm: self.fsm,
            current_term: self.current_term + 1,
            voted_for: Some(self.identity.node_id()),
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            commit_signal: self.commit_signal,
            state,
        }
    }
}

impl RaftNode<Candidate> {
    /// Candidate -> Candidate transition (Triggered by Election Timeout in a
    /// failed term).
    pub fn into_restarted_candidate(self) -> RaftNode<Candidate> {
        let mut state = Candidate::new();
        state.add_vote(self.identity.node_id());

        RaftNode {
            identity: self.identity.clone(),
            fsm: self.fsm,
            current_term: self.current_term + 1,
            voted_for: Some(self.identity.node_id()),
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            commit_signal: self.commit_signal,
            state,
        }
    }

    /// Candidate -> Leader transition (Triggered by Majority Vote).
    pub fn into_leader(self, peer_ids: Vec<NodeId>) -> RaftNode<Leader> {
        let last_log_index = self.last_log_index();
        RaftNode {
            identity: self.identity,
            fsm: self.fsm,
            current_term: self.current_term,
            voted_for: self.voted_for,
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            commit_signal: self.commit_signal,
            state: Leader::new(peer_ids, last_log_index),
        }
    }
}

// --- The Dispatcher Enum (Logical State Machine) ---

#[derive(Debug)]
pub enum RaftNodeState {
    Follower(RaftNode<Follower>),
    Candidate(RaftNode<Candidate>),
    Leader(RaftNode<Leader>),
    Poisoned, // ADR 001: Safety barrier during transition failures
}

macro_rules! delegate_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match $self {
            RaftNodeState::Follower(n) => Ok(n.$method($($args),*)),
            RaftNodeState::Candidate(n) => Ok(n.$method($($args),*)),
            RaftNodeState::Leader(n) => Ok(n.$method($($args),*)),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    };
}

impl RaftNodeState {
    /// Returns the logical identity of the node as a reference to its Arc.
    pub fn identity_arc(&self) -> Result<&Arc<NodeIdentity>, RaftError> {
        delegate_to_inner!(self, identity_arc)
    }

    /// Returns this node's ID.
    pub fn node_id(&self) -> Result<NodeId, RaftError> {
        delegate_to_inner!(self, node_id)
    }

    /// Returns the current term of the node, regardless of its state.
    pub fn current_term(&self) -> Result<Term, RaftError> {
        delegate_to_inner!(self, current_term)
    }

    /// Returns who the node voted for in the current term.
    pub fn voted_for(&self) -> Result<Option<NodeId>, RaftError> {
        delegate_to_inner!(self, voted_for)
    }

    /// Returns the current commit index of the node.
    pub fn commit_index(&self) -> Result<LogIndex, RaftError> {
        delegate_to_inner!(self, commit_index)
    }

    /// Returns the commit signal of the node.
    pub fn commit_signal(&self) -> Result<Arc<Notify>, RaftError> {
        delegate_to_inner!(self, commit_signal).map(|s| s.clone())
    }

    /// Appends a new command to the leader's log.
    /// Returns the index of the newly appended entry.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, RaftError> {
        match self {
            RaftNodeState::Leader(node) => {
                let index = node.last_log_index() + 1;
                let term = node.current_term();
                let entry = LogEntry::new(index, term, command);
                node.log_mut().push(entry);
                Ok(index)
            }
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
            _ => Err(RaftError::NotLeader),
        }
    }

    /// Safely transitions the node state using an ownership-consuming closure.
    pub fn transition<F>(&mut self, f: F)
    where
        F: FnOnce(RaftNodeState) -> RaftNodeState,
    {
        let old_state = std::mem::replace(self, RaftNodeState::Poisoned);
        *self = f(old_state);
    }

    /// Consumes the current state and returns a Follower state for the given
    /// term. This is a universal transition mandated by Raft §5.1.
    pub fn into_follower(self, term: Term, leader_id: Option<NodeId>) -> RaftNodeState {
        let (
            identity,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            commit_signal,
        ) = match self {
            RaftNodeState::Follower(n) => n.into_parts(),
            RaftNodeState::Candidate(n) => n.into_parts(),
            RaftNodeState::Leader(n) => n.into_parts(),
            RaftNodeState::Poisoned => return RaftNodeState::Poisoned,
        };

        let mut new_node = RaftNode {
            identity,
            fsm,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
            commit_signal,
            state: Follower::new(leader_id),
        };

        new_node.set_term(term);
        RaftNodeState::Follower(new_node)
    }

    /// Resets the election timer if the node is a Follower.
    pub fn reset_heartbeat(&mut self) {
        if let RaftNodeState::Follower(node) = self {
            node.state_mut().reset_heartbeat();
        }
    }

    /// Processes a RequestVote RPC.
    ///
    /// Autonomously manages term transitions and vote granting according to
    /// Raft §5.1, §5.2, and §5.4.
    pub fn handle_request_vote(
        &mut self,
        candidate_id: NodeId,
        req_term: Term,
        req_last_log_index: LogIndex,
        req_last_log_term: Term,
    ) -> Result<(Term, bool), RaftError> {
        let current_term = self.current_term()?;

        // 1. If term > currentTerm: set currentTerm = term, transition to follower
        //    (§5.1)
        if req_term > current_term {
            info!(
                "Received higher term ({}) from candidate {}. Transitioning to Follower.",
                req_term, candidate_id
            );
            self.transition(|old| old.into_follower(req_term, None));
            // TODO: Phase 6 - fsync term to sled
        }

        // Re-acquire current state after potential transition
        let mut vote_granted = false;
        let current_term = self.current_term()?;

        match self {
            RaftNodeState::Follower(node) => {
                // 2. If votedFor is null or candidateId, and candidate’s log is at
                // least as up-to-date as receiver’s log, grant vote (§5.2, §5.4)
                if req_term >= node.current_term()
                    && (node.voted_for().is_none() || node.voted_for() == Some(candidate_id))
                {
                    if node.is_log_up_to_date(req_last_log_term, req_last_log_index) {
                        vote_granted = true;
                        node.vote_for(candidate_id);
                        info!(
                            "Granting vote to candidate {} for term {}",
                            candidate_id, req_term
                        );
                    }
                }
            }
            RaftNodeState::Candidate(_) | RaftNodeState::Leader(_) => {
                // If term is equal, we already voted for ourselves or are
                // leading.
            }
            RaftNodeState::Poisoned => return Err(RaftError::Poisoned),
        }

        Ok((current_term, vote_granted))
    }

    /// Processes an AppendEntries RPC.
    ///
    /// Acts as a high-level orchestrator, delegating log reconciliation to
    /// specialized sub-functions. Autonomously manages term transitions and
    /// rival leader detection according to Raft §5.1, §5.2, and §5.3.
    pub async fn handle_append_entries(
        &mut self,
        leader_id: NodeId,
        req_term: Term,
        req_prev_log_index: LogIndex,
        req_prev_log_term: Term,
        entries: Vec<LogEntry>,
        req_leader_commit: LogIndex,
    ) -> Result<(Term, bool, LogIndex), RaftError> {
        let current_term = self.current_term()?;

        // --- Raft RPC Step 1: Term Check (§5.1) ---
        if req_term < current_term {
            debug!(
                "Rejecting AppendEntries from {}: term {} is older than currentTerm {}",
                leader_id, req_term, current_term
            );
            return Ok((current_term, false, LogIndex::ZERO));
        }

        // --- Internal: State Transitions & Heartbeat Verification ---
        if req_term > current_term {
            // §5.1: If term > currentTerm, transition to follower
            info!(
                "Received higher term ({}) from leader {}. Demoting to Follower.",
                req_term, leader_id
            );
            self.transition(|old| old.into_follower(req_term, Some(leader_id)));
            // TODO: Phase 6 - fsync term to sled
        } else if req_term == current_term {
            match self {
                RaftNodeState::Candidate(_) => {
                    // §5.2: If Candidate receives AppendEntries from a leader of the SAME term,
                    // it recognizes the leader as legitimate and returns to follower state.
                    info!(
                        "Candidate recognizing leader {} for term {}. Returning to Follower.",
                        leader_id, req_term
                    );
                    self.transition(|old| old.into_follower(req_term, Some(leader_id)));
                }
                RaftNodeState::Leader(_) => {
                    // FATAL INVARIANT VIOLATION: Two leaders for the same term!
                    // We prioritize Safety over Liveness. Detecting another leader for our own term
                    // implies a failure in the consensus logic or persistence layer. We must halt
                    // to prevent data corruption.
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. Halting \
                         node to prevent state corruption.",
                        leader_id, req_term
                    );
                    error!("{}", msg);
                    panic!("{}", msg);
                }
                RaftNodeState::Follower(node) => {
                    node.state_mut().set_leader_id(Some(leader_id));
                }
                _ => {}
            }
        }

        // --- Raft RPC Steps 2-5: Log Reconciliation (§5.3) ---
        self.reset_heartbeat();

        let (success, last_log_index) = match self {
            RaftNodeState::Follower(node) => {
                node.reconcile_log(
                    req_prev_log_index,
                    req_prev_log_term,
                    entries,
                    req_leader_commit,
                )
                .await
            }
            _ => (false, LogIndex::ZERO), // Should have demoted above
        };

        let current_term = self.current_term()?;
        Ok((current_term, success, last_log_index))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use common::types::ClusterId;
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

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    mod set_commit_index {
        use super::*;

        #[tokio::test]
        async fn correctly_applies_entries_sequentially() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm.clone());

            // 1. Populate log with 3 entries using domain identity helper
            for i in 1..=3 {
                node.log_mut().push(LogEntry::new(
                    LogIndex::new(i),
                    Term::new(1),
                    format!("entry_{}", i).into_bytes(),
                ));
            }

            // 2. Advance commit_index to 2
            node.set_commit_index(LogIndex::new(2)).await;

            // 3. Verify FSM application
            {
                let indices = fsm.applied_indices.lock().unwrap();
                let data = fsm.applied_data.lock().unwrap();
                assert_eq!(indices.len(), 2);
                assert_eq!(indices[0], LogIndex::new(1));
                assert_eq!(indices[1], LogIndex::new(2));
                assert_eq!(data[0], b"entry_1");
                assert_eq!(data[1], b"entry_2");
            }
            assert_eq!(node.last_applied(), LogIndex::new(2));

            // 4. Advance commit_index to 3
            node.set_commit_index(LogIndex::new(3)).await;

            // 5. Verify incremental FSM application
            {
                let indices = fsm.applied_indices.lock().unwrap();
                assert_eq!(indices.len(), 3);
                assert_eq!(indices[2], LogIndex::new(3));
            }
            assert_eq!(node.last_applied(), LogIndex::new(3));
        }

        #[tokio::test]
        async fn ignores_stale_commit_index() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm.clone());

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            node.set_commit_index(LogIndex::new(1)).await;
            assert_eq!(node.commit_index(), LogIndex::new(1));

            // Try to set back to 0
            node.set_commit_index(LogIndex::new(0)).await;
            assert_eq!(node.commit_index(), LogIndex::new(1));

            // FSM should only have 1 application
            assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL: Protocol violation")]
        async fn panics_on_invalid_log_boundary() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            // Log is empty, so last_log_index is 0.
            // Attempting to commit 1 should panic.
            node.set_commit_index(LogIndex::new(1)).await;
        }
    }

    mod is_log_up_to_date {
        use super::*;

        #[test]
        fn returns_true_when_candidate_term_is_higher() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            // Local: (index 1, term 1)
            // Candidate: (index 1, term 2)
            assert!(node.is_log_up_to_date(Term::new(2), LogIndex::new(1)));
        }

        #[test]
        fn returns_false_when_candidate_term_is_lower() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(2), vec![]));

            // Local: (index 1, term 2)
            // Candidate: (index 10, term 1)
            assert!(!node.is_log_up_to_date(Term::new(1), LogIndex::new(10)));
        }

        #[test]
        fn returns_true_when_same_term_and_candidate_log_is_longer() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            // Local: (index 1, term 1)
            // Candidate: (index 2, term 1)
            assert!(node.is_log_up_to_date(Term::new(1), LogIndex::new(2)));
        }

        #[test]
        fn returns_true_when_same_term_and_candidate_log_is_equal_length() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            // Local: (index 1, term 1)
            // Candidate: (index 1, term 1)
            assert!(node.is_log_up_to_date(Term::new(1), LogIndex::new(1)));
        }

        #[test]
        fn returns_false_when_same_term_and_candidate_log_is_shorter() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(2), Term::new(1), vec![]));

            // Local: (index 2, term 1)
            // Candidate: (index 1, term 1)
            assert!(!node.is_log_up_to_date(Term::new(1), LogIndex::new(1)));
        }
    }

    mod handle_request_vote {
        use super::*;

        #[test]
        fn grants_vote_when_term_is_higher_and_not_voted() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));

            let (term, granted) = state
                .handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(granted);
            assert_eq!(term, Term::new(1));
            assert_eq!(state.voted_for().unwrap(), Some(NodeId::new(2)));
        }

        #[test]
        fn grants_vote_when_term_is_current_and_not_voted() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));
            state.transition(|old| old.into_follower(Term::new(1), None));

            let (term, granted) = state
                .handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(granted);
            assert_eq!(term, Term::new(1));
        }

        #[test]
        fn rejects_vote_when_term_is_older() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));
            state.transition(|old| old.into_follower(Term::new(2), None));

            let (term, granted) = state
                .handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(!granted);
            assert_eq!(term, Term::new(2));
        }

        #[test]
        fn rejects_vote_when_already_voted_for_different_candidate() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));
            state
                .handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            let (term, granted) = state
                .handle_request_vote(NodeId::new(3), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(!granted);
            assert_eq!(term, Term::new(1));
            assert_eq!(state.voted_for().unwrap(), Some(NodeId::new(2)));
        }

        #[test]
        fn rejects_vote_when_candidate_log_is_stale() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));
            let mut state = RaftNodeState::Follower(node);

            let (term, granted) = state
                .handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(!granted);
            assert_eq!(term, Term::new(1));
        }

        #[test]
        fn demotes_to_follower_on_higher_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let follower = RaftNode::<Follower>::new(id, fsm);
            let candidate = follower.into_candidate(); // Term 1
            let mut state = RaftNodeState::Candidate(candidate);

            let (term, granted) = state
                .handle_request_vote(NodeId::new(2), Term::new(2), LogIndex::new(0), Term::new(0))
                .unwrap();

            assert!(granted);
            assert_eq!(term, Term::new(2));
            assert!(matches!(state, RaftNodeState::Follower(_)));
        }
    }

    mod handle_append_entries {
        use super::*;

        #[tokio::test]
        async fn returns_success_when_term_is_current() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));

            let (term, success, _) = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::ZERO,
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await
                .unwrap();

            assert!(success);
            assert_eq!(term, Term::ZERO);
        }

        #[tokio::test]
        async fn rejects_when_term_is_older() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut state = RaftNodeState::Follower(RaftNode::<Follower>::new(id, fsm));
            state.transition(|old| old.into_follower(Term::new(2), None));

            let (term, success, _) = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1),
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await
                .unwrap();

            assert!(!success);
            assert_eq!(term, Term::new(2));
        }

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let follower = RaftNode::<Follower>::new(id, fsm);
            let candidate = follower.into_candidate(); // Term 1
            let mut state = RaftNodeState::Candidate(candidate);

            let (term, success, _) = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1),
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await
                .unwrap();

            assert!(success);
            assert_eq!(term, Term::new(1));
            assert!(matches!(state, RaftNodeState::Follower(_)));
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn panics_on_rival_leader_same_term() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let follower = RaftNode::<Follower>::new(id, fsm);
            let candidate = follower.into_candidate();
            let leader = candidate.into_leader(vec![NodeId::new(2)]); // Term 1
            let mut state = RaftNodeState::Leader(leader);

            let _ = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1),
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await;
        }
    }

    mod reconcile_log {
        use super::*;

        #[tokio::test]
        async fn rejects_when_prev_log_index_term_mismatches() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            // Log has (index 1, term 1). Request expects (index 1, term 2)
            let (success, _) = node
                .reconcile_log(LogIndex::new(1), Term::new(2), vec![], LogIndex::ZERO)
                .await;

            assert!(!success);
        }

        #[tokio::test]
        async fn truncates_and_appends_on_conflict() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(2), Term::new(1), vec![]));

            // Conflict at index 2: local term 1, remote term 2
            let entries = vec![LogEntry::new(LogIndex::new(2), Term::new(2), vec![])];
            let (success, last_idx) = node
                .reconcile_log(LogIndex::new(1), Term::new(1), entries, LogIndex::ZERO)
                .await;

            assert!(success);
            assert_eq!(last_idx, LogIndex::new(2));
            assert_eq!(node.log().len(), 2);
            assert_eq!(node.get_term_at(LogIndex::new(2)), Term::new(2));
        }

        #[tokio::test]
        async fn appends_new_entries_contiguously() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);

            let entries = vec![
                LogEntry::new(LogIndex::new(1), Term::new(1), vec![]),
                LogEntry::new(LogIndex::new(2), Term::new(1), vec![]),
            ];
            let (success, last_idx) = node
                .reconcile_log(LogIndex::ZERO, Term::ZERO, entries, LogIndex::ZERO)
                .await;

            assert!(success);
            assert_eq!(last_idx, LogIndex::new(2));
            assert_eq!(node.log().len(), 2);
        }

        #[tokio::test]
        async fn advances_commit_index() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let mut node = RaftNode::<Follower>::new(id, fsm);
            node.log_mut()
                .push(LogEntry::new(LogIndex::new(1), Term::new(1), vec![]));

            node.reconcile_log(
                LogIndex::ZERO,
                Term::ZERO,
                vec![],
                LogIndex::new(1), // leaderCommit=1
            )
            .await;

            assert_eq!(node.commit_index(), LogIndex::new(1));
        }
    }
}
