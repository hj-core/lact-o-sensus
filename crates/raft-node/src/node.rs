use std::sync::Arc;

use crate::identity::NodeIdentity;

// --- Type-State Markers (Role-Specific Volatile State) ---

#[derive(Debug, Default)]
pub struct Follower {
    leader_id: Option<u64>,
}

impl Follower {
    pub fn leader_id(&self) -> Option<u64> {
        self.leader_id
    }
}

#[derive(Debug)]
pub struct Candidate {
    // Phase 3: Will hold votes_received HashSet
}

#[derive(Debug)]
pub struct Leader {
    // Phase 3: Will hold next_index and match_index Maps
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

    pub fn state(&self) -> &S {
        &self.state
    }

    /// Safely updates the term. Per Raft safety rules, terms must only
    /// increase.
    pub fn update_term(&mut self, new_term: u64) {
        if new_term > self.current_term {
            self.current_term = new_term;
            // TODO: Phase 5 - fsync to sled before acknowledging RPC
        }
    }
}

impl RaftNode<Follower> {
    pub fn new(identity: Arc<NodeIdentity>) -> Self {
        Self {
            identity,
            current_term: 0,
            state: Follower::default(),
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
