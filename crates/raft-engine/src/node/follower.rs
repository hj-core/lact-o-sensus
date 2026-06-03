//! Follower role implementation for the Raft engine.
//!
//! This module defines the passive behavior of nodes, including log
//! reconciliation (§5.3), heartbeat monitoring (ADR 003), and vote
//! evaluation (§5.2).

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

use super::Candidate;
use super::NodeState;
use super::RaftNode;
use super::TickAction;
use crate::storage::LogStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

/// Passive role responsible for log reconciliation and heartbeat tracking.
///
/// Follower nodes maintain liveness by monitoring heartbeats from the leader
/// (ADR 003). If the heartbeat timer expires, the node transitions to the
/// Candidate role to initiate an election.
#[derive(Debug)]
pub struct Follower {
    leader_id: Option<NodeId>,
    last_heartbeat: Tick,
    timeout: TickDuration,
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
        let current_term = self.current_term()?;
        let voted_for = self.voted_for()?;

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
        let current_term = self.current_term()?;
        let mut node = self.transition(Candidate::new(election_start, timeout));

        let new_term = (current_term + 1)?;
        let node_id = node.node_id();
        node.advance_term_and_vote(new_term, node_id)?;
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
                self.truncate_log(entry_index)?;
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
            self.append_entries(to_append)?;
        }

        Ok(true)
    }

    pub(crate) fn reconcile_last_committed(
        &mut self,
        leader_commit: LogIndex,
    ) -> Result<(), NodeError> {
        if leader_commit > self.last_committed() {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common::proto::v1::raft::LogEntry;
    use common::raft_api::StateMachine;
    use common::types::LogIndex;
    use common::types::NodeId;
    use common::types::Term;
    use common::types::errors::FsmError;

    use super::*;
    use crate::node::test_utils::*;
    use crate::storage::MemoryStorage;

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
            use common::types::trace::TraceId;

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

            fn setup_node_with_log(last_idx: u64, last_term: u64) -> RaftNode<Follower, MockFsm> {
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
