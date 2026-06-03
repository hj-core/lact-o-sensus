//! Candidate role implementation for the Raft engine.
//!
//! This module defines the behavior of nodes during leadership campaigns,
//! including vote solicitation, quorum tracking, and role transitions
//! (ADR 002).

use std::collections::HashSet;

use common::raft_api::StateMachine;
use common::types::NodeId;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::info;

use super::Leader;
use super::NodeState;
use super::RaftNode;
use super::TickAction;
use crate::tick::Tick;
use crate::tick::TickDuration;

/// Active role during leadership campaigns.
///
/// Candidate nodes solicit votes from peers and transition to the Leader role
/// upon reaching a quorum (ADR 002). If an election times out before a quorum
/// is reached, the term is incremented and a new campaign is initiated.
#[derive(Debug)]
pub struct Candidate {
    votes_received: HashSet<NodeId>,
    election_start: Tick,
    timeout: TickDuration,
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
        let current_term = self.current_term()?;
        let mut node = self.transition(Candidate::new(election_start, timeout));

        let new_term = (current_term + 1)?;
        let node_id = node.node_id();
        node.advance_term_and_vote(new_term, node_id)?;
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
        let node = self.transition(Leader::new(peer_ids, last_log_index, last_heartbeat)?);

        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            term = %term,
            "Role Transition: -> Leader"
        );

        Ok(node)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common::proto::v1::raft::LogEntry;
    use common::types::LogIndex;

    use super::*;
    use crate::node::test_utils::*;
    use crate::storage::LogStorage;
    use crate::storage::MemoryStorage;

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
