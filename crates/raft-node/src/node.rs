use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use common::types::NodeId;

use crate::identity::NodeIdentity;

// --- Type-State Markers (Role-Specific Volatile State) ---

#[derive(Debug)]
pub struct Follower {
    /// The ID of the current leader (if known).
    leader_id: Option<NodeId>,

    /// The instant when the last valid heartbeat was received.
    last_heartbeat: Instant,
}

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

    /// Resets the heartbeat timer.
    pub fn reset_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
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
    pub fn new() -> Self {
        Self::default()
    }
}

pub trait NodeState: std::fmt::Debug {}
impl NodeState for Follower {}
impl NodeState for Candidate {}
impl NodeState for Leader {}

// --- Generic Node Struct (Persistent State) ---

#[derive(Debug)]
pub struct RaftNode<S: NodeState> {
    /// Verified identity of the node (ADR 004).
    identity: Arc<NodeIdentity>,

    /// Persistent term across role transitions.
    current_term: u64,

    /// CandidateId that received vote in current term (or None if none).
    voted_for: Option<NodeId>,

    /// Role-specific volatile state.
    state: S,
}

impl<S: NodeState> RaftNode<S> {
    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    pub fn current_term(&self) -> u64 {
        self.current_term
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
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

    /// Universal transition to Follower (can happen from any state if a higher
    /// term is seen).
    pub fn into_follower(self, term: u64, leader_id: Option<NodeId>) -> RaftNode<Follower> {
        let mut node = RaftNode {
            identity: self.identity,
            current_term: self.current_term,
            voted_for: self.voted_for,
            state: Follower::new(leader_id),
        };
        node.set_term(term);
        node
    }
}

impl RaftNode<Follower> {
    pub fn new(identity: Arc<NodeIdentity>) -> Self {
        Self {
            identity,
            current_term: 0,
            voted_for: None,
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
            state,
        }
    }

    /// Candidate -> Leader transition (Triggered by Majority Vote).
    pub fn into_leader(self) -> RaftNode<Leader> {
        RaftNode {
            identity: self.identity,
            current_term: self.current_term,
            voted_for: self.voted_for,
            state: Leader::new(),
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
    /// Safely transitions the node state using an ownership-consuming closure.
    /// This is the bridge between the Type-State pattern and the RwLock
    /// storage.
    pub fn transition<F>(&mut self, f: F)
    where
        F: FnOnce(RaftNodeState) -> RaftNodeState,
    {
        let old_state = std::mem::replace(self, RaftNodeState::Poisoned);
        *self = f(old_state);
    }
}
