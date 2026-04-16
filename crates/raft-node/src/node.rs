use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use common::proto::v1::LogEntry;
use common::types::ClusterId;
use common::types::NodeId;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::debug;

use crate::identity::NodeIdentity;

#[derive(Error, Debug)]
pub enum RaftError {
    #[error("Node is in an unrecoverable state due to a failed transition")]
    Poisoned,
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
    next_index: HashMap<NodeId, u64>,

    /// For each server, index of highest log entry known to be replicated on
    /// server (initialized to 0, increases monotonically).
    match_index: HashMap<NodeId, u64>,
}

impl Leader {
    /// Initializes leader-specific volatile state.
    ///
    /// nextIndex is initialized to (lastLogIndex + 1), and matchIndex is
    /// initialized to 0 for all peers.
    pub fn new(peer_ids: Vec<NodeId>, last_log_index: u64) -> Self {
        let mut next_index = HashMap::new();
        let mut match_index = HashMap::new();

        for peer_id in peer_ids {
            next_index.insert(peer_id, last_log_index + 1);
            match_index.insert(peer_id, 0);
        }

        Self {
            next_index,
            match_index,
        }
    }

    pub fn next_index(&self) -> &HashMap<NodeId, u64> {
        &self.next_index
    }

    pub fn next_index_mut(&mut self) -> &mut HashMap<NodeId, u64> {
        &mut self.next_index
    }

    pub fn match_index(&self) -> &HashMap<NodeId, u64> {
        &self.match_index
    }

    pub fn match_index_mut(&mut self) -> &mut HashMap<NodeId, u64> {
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

    // --- Persistent State ---
    /// Persistent term across role transitions.
    current_term: u64,

    /// CandidateId that received vote in current term (or None if none).
    voted_for: Option<NodeId>,

    /// The replicated log (1-based indexing used logically).
    log: Vec<LogEntry>,

    // --- Volatile State (Shared) ---
    /// Index of highest log entry known to be committed.
    commit_index: u64,

    /// Index of highest log entry applied to state machine.
    last_applied: u64,

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

    pub fn current_term(&self) -> u64 {
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

    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }

    /// Updates the commit index.
    ///
    /// Adheres to the monotonicity requirement: stale updates (e.g., from
    /// delayed heartbeats) are ignored.
    ///
    /// # Panics
    /// Panics if the new index exceeds the current log boundaries, which
    /// indicates a fundamental protocol violation.
    pub fn set_commit_index(&mut self, index: u64) {
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

        self.commit_index = index;
    }

    pub fn last_applied(&self) -> u64 {
        self.last_applied
    }

    /// Updates the last applied index.
    ///
    /// # Panics
    /// Panics if the new index regresses or exceeds the commit index, as
    /// the state machine must strictly follow the committed log.
    pub fn set_last_applied(&mut self, index: u64) {
        if index < self.last_applied {
            panic!(
                "CRITICAL: State machine regression. Attempted to set last_applied to {} but \
                 current is {}",
                index, self.last_applied
            );
        }

        if index > self.commit_index {
            panic!(
                "CRITICAL: State machine violation. Attempted to apply index {} but commit_index \
                 is only {}",
                index, self.commit_index
            );
        }

        self.last_applied = index;
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// Returns the index of the last entry in the log (0 if empty).
    pub fn last_log_index(&self) -> u64 {
        self.log.last().map(|e| e.index).unwrap_or(0)
    }

    /// Returns the term of the last entry in the log (0 if empty).
    pub fn last_log_term(&self) -> u64 {
        self.log.last().map(|e| e.term).unwrap_or(0)
    }

    /// Returns the term of the log entry at the given index.
    /// Returns 0 if index is 0 or out of bounds.
    pub fn get_term_at(&self, index: u64) -> u64 {
        if index == 0 {
            return 0;
        }
        // Assuming contiguous log entries starting at index 1.
        self.log
            .get((index - 1) as usize)
            .map(|e| e.term)
            .unwrap_or(0)
    }

    /// Internal helper to update term and reset vote.
    fn set_term(&mut self, term: u64) {
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
        }
    }

    /// Records a vote for a candidate in the current term.
    pub fn vote_for(&mut self, candidate_id: NodeId) {
        self.voted_for = Some(candidate_id);
        // TODO: Phase 5 - fsync to sled
    }

    /// Decomposes the node into its transition-invariant components.
    fn into_parts(
        self,
    ) -> (
        Arc<NodeIdentity>,
        u64,
        Option<NodeId>,
        Vec<LogEntry>,
        u64,
        u64,
    ) {
        (
            self.identity,
            self.current_term,
            self.voted_for,
            self.log,
            self.commit_index,
            self.last_applied,
        )
    }
}

impl RaftNode<Follower> {
    pub fn new(identity: Arc<NodeIdentity>) -> Self {
        Self {
            identity,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            state: Follower::default(),
        }
    }

    /// Follower -> Candidate transition (Triggered by Election Timeout).
    pub fn into_candidate(self) -> RaftNode<Candidate> {
        let mut state = Candidate::new();
        state.add_vote(self.identity.node_id());

        RaftNode {
            identity: self.identity.clone(),
            current_term: self.current_term + 1,
            voted_for: Some(self.identity.node_id()),
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
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
            current_term: self.current_term + 1,
            voted_for: Some(self.identity.node_id()),
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            state,
        }
    }

    /// Candidate -> Leader transition (Triggered by Majority Vote).
    pub fn into_leader(self, peer_ids: Vec<NodeId>) -> RaftNode<Leader> {
        let last_log_index = self.last_log_index();
        RaftNode {
            identity: self.identity,
            current_term: self.current_term,
            voted_for: self.voted_for,
            log: self.log,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
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

impl RaftNodeState {
    /// Returns the logical identity of the node as a reference to its Arc.
    pub fn identity_arc(&self) -> Result<&Arc<NodeIdentity>, RaftError> {
        match self {
            RaftNodeState::Follower(n) => Ok(n.identity_arc()),
            RaftNodeState::Candidate(n) => Ok(n.identity_arc()),
            RaftNodeState::Leader(n) => Ok(n.identity_arc()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    }

    /// Returns the current term of the node, regardless of its state.
    pub fn current_term(&self) -> Result<u64, RaftError> {
        match self {
            RaftNodeState::Follower(node) => Ok(node.current_term()),
            RaftNodeState::Candidate(node) => Ok(node.current_term()),
            RaftNodeState::Leader(node) => Ok(node.current_term()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    }

    /// Returns who the node voted for in the current term.
    pub fn voted_for(&self) -> Result<Option<NodeId>, RaftError> {
        match self {
            RaftNodeState::Follower(node) => Ok(node.voted_for()),
            RaftNodeState::Candidate(node) => Ok(node.voted_for()),
            RaftNodeState::Leader(node) => Ok(node.voted_for()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    }

    /// Returns the current commit index of the node.
    pub fn commit_index(&self) -> Result<u64, RaftError> {
        match self {
            RaftNodeState::Follower(node) => Ok(node.commit_index()),
            RaftNodeState::Candidate(node) => Ok(node.commit_index()),
            RaftNodeState::Leader(node) => Ok(node.commit_index()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    }

    /// Returns the cluster ID this node belongs to.
    pub fn cluster_id(&self) -> Result<&ClusterId, RaftError> {
        match self {
            RaftNodeState::Follower(n) => Ok(n.identity().cluster_id()),
            RaftNodeState::Candidate(n) => Ok(n.identity().cluster_id()),
            RaftNodeState::Leader(n) => Ok(n.identity().cluster_id()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
        }
    }

    /// Returns this node's ID.
    pub fn node_id(&self) -> Result<NodeId, RaftError> {
        match self {
            RaftNodeState::Follower(n) => Ok(n.identity().node_id()),
            RaftNodeState::Candidate(n) => Ok(n.identity().node_id()),
            RaftNodeState::Leader(n) => Ok(n.identity().node_id()),
            RaftNodeState::Poisoned => Err(RaftError::Poisoned),
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
    pub fn into_follower(self, term: u64, leader_id: Option<NodeId>) -> RaftNodeState {
        let (identity, current_term, voted_for, log, commit_index, last_applied) = match self {
            RaftNodeState::Follower(n) => n.into_parts(),
            RaftNodeState::Candidate(n) => n.into_parts(),
            RaftNodeState::Leader(n) => n.into_parts(),
            RaftNodeState::Poisoned => return RaftNodeState::Poisoned,
        };

        let mut new_node = RaftNode {
            identity,
            current_term,
            voted_for,
            log,
            commit_index,
            last_applied,
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
}
