use std::collections::HashSet;

use common::raft_api::StateMachine;
use common::types::NodeId;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::info;

use crate::tick::Tick;
use crate::tick::TickDuration;

use super::Leader;
use super::NodeState;
use super::RaftNode;
use super::TickAction;

/// Active role during leadership campaigns.
///
/// Candidate nodes solicit votes from peers and transition to the Leader role
/// upon reaching a quorum (ADR 002). If an election times out before a quorum
/// is reached, the term is incremented and a new campaign is initiated.
#[derive(Debug)]
pub struct Candidate {
    pub(super) votes_received: HashSet<NodeId>,
    pub(super) election_start: Tick,
    pub(super) timeout: TickDuration,
}

impl Candidate {
    pub fn new(election_start: Tick, timeout: TickDuration) -> Self {
        Self {
            votes_received: HashSet::new(),
            election_start,
            timeout,
        }
    }

    pub fn election_start(&self) -> Tick {
        self.election_start
    }

    pub fn timeout(&self) -> TickDuration {
        self.timeout
    }

    pub fn add_vote(&mut self, peer_id: NodeId) {
        self.votes_received.insert(peer_id);
    }

    pub fn vote_count(&self) -> usize {
        self.votes_received.len()
    }

    pub fn evaluate_tick(&self, now: Tick) -> TickAction {
        if now - self.election_start >= self.timeout {
            TickAction::StartElection
        } else {
            TickAction::None
        }
    }
}

impl NodeState for Candidate {}

impl<S: StateMachine> RaftNode<Candidate, S> {
    pub fn try_into_restarted_candidate(
        self,
        election_start: Tick,
        timeout: TickDuration,
    ) -> Result<RaftNode<Candidate, S>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Candidate::new(election_start, timeout),
        };

        let new_term = (node.current_term()? + 1)?;
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

    pub fn try_into_leader(
        self,
        peer_ids: Vec<NodeId>,
        last_heartbeat: Tick,
    ) -> Result<RaftNode<Leader, S>, NodeError> {
        let last_log_index = self.last_log_index()?;
        let term = self.current_term()?;
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Leader::new(peer_ids, last_log_index, last_heartbeat)?,
        };

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            term = %term,
            "Role Transition: -> Leader"
        );

        Ok(node)
    }
}
