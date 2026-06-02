use std::cmp;
use std::sync::Arc;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
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

use super::Candidate;
use super::NodeState;
use super::RaftNode;
use super::ReconciliationResult;
use super::TickAction;

/// Passive role responsible for log reconciliation and heartbeat tracking.
///
/// Follower nodes maintain liveness by monitoring heartbeats from the leader
/// (ADR 003). If the heartbeat timer expires, the node transitions to the
/// Candidate role to initiate an election.
#[derive(Debug)]
pub struct Follower {
    pub(super) leader_id: Option<NodeId>,
    pub(super) last_heartbeat: Tick,
    pub(super) timeout: TickDuration,
}

impl Follower {
    pub fn new(leader_id: Option<NodeId>, last_heartbeat: Tick, timeout: TickDuration) -> Self {
        Self {
            leader_id,
            last_heartbeat,
            timeout,
        }
    }

    pub fn leader_id(&self) -> Option<NodeId> {
        self.leader_id
    }

    pub fn last_heartbeat(&self) -> Tick {
        self.last_heartbeat
    }

    pub fn timeout(&self) -> TickDuration {
        self.timeout
    }

    pub fn set_leader_id(&mut self, leader_id: Option<NodeId>) {
        self.leader_id = leader_id;
    }

    pub fn reset_heartbeat(&mut self, current_tick: Tick) {
        self.last_heartbeat = current_tick;
    }

    pub fn evaluate_tick(&self, now: Tick) -> TickAction {
        if now - self.last_heartbeat >= self.timeout {
            TickAction::StartElection
        } else {
            TickAction::None
        }
    }
}

impl NodeState for Follower {}

impl<S: StateMachine> RaftNode<Follower, S> {
    pub fn try_new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<S>,
        log_store: Arc<dyn LogStorage>,
        last_heartbeat: Tick,
        timeout: TickDuration,
    ) -> Result<Self, NodeError> {
        let last_applied = fsm.last_applied_index().map_err(|e| e.into())?;
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
            state: Follower::new(None, last_heartbeat, timeout),
        })
    }

    /// Following Raft §5.3, reconciles the local log with entries from the
    /// leader.
    #[instrument(
        name = "log_reconciliation",
        target = "raft::replication",
        skip_all,
        fields(prev_index = %prev_log_index, entry_count = entries.len())
    )]
    pub fn reconcile_log(
        &mut self,
        prev_log_index: LogIndex,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: LogIndex,
    ) -> Result<ReconciliationResult, NodeError> {
        if !self.verify_log_consistency(prev_log_index, prev_log_term)? {
            return Ok(ReconciliationResult::mismatch(self.last_log_index()?));
        }

        if !self.append_entries_with_reconciliation(entries)? {
            return Ok(ReconciliationResult::mismatch(self.last_log_index()?));
        }

        self.reconcile_last_committed(leader_commit)?;
        Ok(ReconciliationResult::success(self.last_log_index()?))
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
    pub fn try_into_candidate(
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
            "Role Transition: -> Candidate"
        );

        Ok(node)
    }

    // --- Follower Helpers ---

    pub(crate) fn verify_log_consistency(
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
                target: ClinicalTarget::RaftReplication.as_str(),
                index = %prev_log_index,
                last_log_index = %last_idx,
                "Rejecting AppendEntries: prevLogIndex is beyond local log length"
            );
            return Ok(false);
        }

        let local_term = self.get_term_at(prev_log_index)?;
        if local_term != prev_log_term {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                index = %prev_log_index,
                local_term = %local_term,
                remote_term = %prev_log_term,
                "Rejecting AppendEntries: prevLogIndex has term mismatch"
            );
            return Ok(false);
        }

        Ok(true)
    }

    pub(crate) fn append_entries_with_reconciliation(
        &mut self,
        entries: Vec<LogEntry>,
    ) -> Result<bool, NodeError> {
        for entry in &entries {
            let entry_index = LogIndex::new(entry.index);
            let local_term = self.get_term_at(entry_index)?;
            if local_term != Term::ZERO && local_term != Term::new(entry.term) {
                info!(
                    target: ClinicalTarget::RaftReplication.as_str(),
                    index = %entry_index,
                    "Log conflict detected. Truncating log."
                );
                self.log_store
                    .truncate_log(entry_index)
                    .map_err(NodeError::from)?;
                break;
            }
        }

        let mut to_append = Vec::new();
        let mut next_expected = (self.last_log_index()? + 1)?;

        for entry in entries {
            let entry_index = LogIndex::new(entry.index);
            if entry_index >= next_expected {
                if entry_index != next_expected {
                    error!(
                        target: ClinicalTarget::RaftReplication.as_str(),
                        index = %entry_index,
                        expected = %next_expected,
                        "Non-contiguous log append attempted by Leader."
                    );
                    return Ok(false);
                }
                to_append.push(entry);
                next_expected = (next_expected + 1)?;
            }
        }

        if !to_append.is_empty() {
            self.log_store
                .append_entries(to_append)
                .map_err(NodeError::from)?;
        }

        Ok(true)
    }

    pub(crate) fn reconcile_last_committed(&mut self, leader_commit: LogIndex) -> Result<(), NodeError> {
        if leader_commit > self.last_committed {
            let last_new_idx = self.last_log_index()?;
            let new_commit = cmp::min(leader_commit, last_new_idx);
            self.advance_last_committed(new_commit)?;
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                index = %new_commit,
                "Updated last_committed"
            );
        }
        Ok(())
    }

    pub(crate) fn is_log_up_to_date(
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
