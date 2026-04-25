use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use thiserror::Error;
use tracing::debug;
use tracing::error;
use tracing::info;

pub use crate::node::Candidate;
pub use crate::node::ConsensusProgress;
pub use crate::node::Follower;
pub use crate::node::Leader;
pub use crate::node::RaftNode;

#[derive(Error, Debug)]
pub enum ConsensusError {
    #[error("Node is not the leader")]
    NotLeader,
}

/// The Dispatcher Enum (Logical State Machine).
///
/// This is the primary entry point for all consensus operations, managing
/// the transitions between Raft roles (ADR 002).
#[derive(Debug)]
pub enum LogicalNode {
    Follower(RaftNode<Follower>),
    Candidate(RaftNode<Candidate>),
    Leader(RaftNode<Leader>),
    Poisoned, // ADR 001: Safety barrier during transition failures
}

macro_rules! delegate_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match $self {
            LogicalNode::Follower(n) => n.$method($($args),*),
            LogicalNode::Candidate(n) => n.$method($($args),*),
            LogicalNode::Leader(n) => n.$method($($args),*),
            LogicalNode::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendEntriesResult {
    pub term: Term,
    pub success: bool,
    pub conflict_index: LogIndex,
}

impl AppendEntriesResult {
    /// Terminal rejection due to a stale term (§5.1).
    pub fn stale_term(term: Term) -> Self {
        Self {
            term,
            success: false,
            conflict_index: LogIndex::ZERO,
        }
    }

    /// Failure due to log inconsistency at prevLogIndex/Term (§5.3).
    pub fn inconsistent(term: Term, last_index: LogIndex) -> Self {
        Self {
            term,
            success: false,
            conflict_index: last_index,
        }
    }

    /// Successful reconciliation and append (§5.3).
    pub fn success(term: Term, last_index: LogIndex) -> Self {
        Self {
            term,
            success: true,
            conflict_index: last_index,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestVoteResult {
    pub term: Term,
    pub vote_granted: bool,
}

impl RequestVoteResult {
    /// Rejection due to stale term or outdated log (§5.1, §5.4).
    pub fn rejected(term: Term) -> Self {
        Self {
            term,
            vote_granted: false,
        }
    }

    /// Successful vote grant (§5.2).
    pub fn granted(term: Term) -> Self {
        Self {
            term,
            vote_granted: true,
        }
    }
}

// =============================================================================
// Implementation: LogicalNode (High-Level Protocol Orchestrator)
// =============================================================================

impl LogicalNode {
    /// Processes an AppendEntries RPC.
    pub async fn handle_append_entries(
        &mut self,
        leader_id: NodeId,
        req_term: Term,
        req_prev_log_index: LogIndex,
        req_prev_log_term: Term,
        entries: Vec<LogEntry>,
        req_leader_commit: LogIndex,
    ) -> AppendEntriesResult {
        let current_term = self.current_term();

        // 1. Term check (§5.1)
        if req_term < current_term {
            debug!(
                "Rejecting AppendEntries from {}: term {} is older than currentTerm {}",
                leader_id, req_term, current_term
            );
            return AppendEntriesResult::stale_term(current_term);
        }

        // 2. High-level state transitions and demotions (§5.1, §5.2)
        if req_term > current_term {
            info!(
                "Received higher term ({}) from leader {}. Demoting to Follower.",
                req_term, leader_id
            );
            self.transition(|old| old.into_follower(req_term, Some(leader_id)));
        } else if req_term == current_term {
            match self {
                LogicalNode::Candidate(_) => {
                    info!(
                        "Candidate recognizing leader {} for term {}. Returning to Follower.",
                        leader_id, req_term
                    );
                    self.transition(|old| old.into_follower(req_term, Some(leader_id)));
                }
                LogicalNode::Leader(_) => {
                    self.transition(|_| {
                        let msg = format!(
                            "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. \
                             Halting node.",
                            leader_id, req_term
                        );
                        error!("{}", msg);
                        panic!("{}", msg);
                    });
                }
                LogicalNode::Follower(node) => {
                    node.state_mut().set_leader_id(Some(leader_id));
                }
                _ => {}
            }
        }

        // 3. Delegation to physical reconciliation (§5.3)
        self.reset_heartbeat();

        match self {
            LogicalNode::Follower(node) => {
                let result = node
                    .reconcile_log(
                        req_prev_log_index,
                        req_prev_log_term,
                        entries,
                        req_leader_commit,
                    )
                    .await;

                if result.success {
                    AppendEntriesResult::success(self.current_term(), result.last_index)
                } else {
                    AppendEntriesResult::inconsistent(self.current_term(), result.last_index)
                }
            }
            _ => AppendEntriesResult::inconsistent(self.current_term(), LogIndex::ZERO),
        }
    }

    /// Processes a RequestVote RPC.
    pub fn handle_request_vote(
        &mut self,
        candidate_id: NodeId,
        req_term: Term,
        req_last_log_index: LogIndex,
        req_last_log_term: Term,
    ) -> RequestVoteResult {
        let current_term = self.current_term();

        // 1. High-level term update (§5.1)
        if req_term > current_term {
            info!(
                "Received higher term ({}) from candidate {}. Transitioning to Follower.",
                req_term, candidate_id
            );
            self.transition(|old| old.into_follower(req_term, None));
        }

        // 2. Delegate vote granting logic to physical foundation (§5.2, §5.4)
        let vote_granted = match self {
            LogicalNode::Follower(node) => node.attempt_grant_vote(
                candidate_id,
                req_term,
                req_last_log_index,
                req_last_log_term,
            ),
            _ => false,
        };

        if vote_granted {
            info!(
                "Granting vote to candidate {} for term {}",
                candidate_id, req_term
            );
            RequestVoteResult::granted(self.current_term())
        } else {
            RequestVoteResult::rejected(self.current_term())
        }
    }

    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, ConsensusError> {
        match self {
            LogicalNode::Leader(node) => Ok(node.propose(command)),
            LogicalNode::Poisoned => panic!("Halt Mandate: Node is poisoned"),
            _ => Err(ConsensusError::NotLeader),
        }
    }

    /// Consumes the current state and returns a Follower state for the given
    /// term. This is a universal transition mandated by Raft §5.1.
    pub fn into_follower(self, term: Term, leader_id: Option<NodeId>) -> LogicalNode {
        match self {
            LogicalNode::Follower(n) => LogicalNode::Follower(n.into_follower(term, leader_id)),
            LogicalNode::Candidate(n) => LogicalNode::Follower(n.into_follower(term, leader_id)),
            LogicalNode::Leader(n) => LogicalNode::Follower(n.into_follower(term, leader_id)),
            LogicalNode::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        }
    }

    /// Safely transitions the node state using an ownership-consuming closure.
    pub fn transition<F>(&mut self, f: F)
    where
        F: FnOnce(LogicalNode) -> LogicalNode,
    {
        let old_state = std::mem::replace(self, LogicalNode::Poisoned);
        *self = f(old_state);
    }

    /// Resets the election timer if the node is a Follower.
    pub fn reset_heartbeat(&mut self) {
        if let LogicalNode::Follower(node) = self {
            node.state_mut().reset_heartbeat();
        }
    }

    // --- Infallible Accessors (Halt Mandate enforced via panic on Poisoned) ---

    pub fn is_poisoned(&self) -> bool {
        matches!(self, LogicalNode::Poisoned)
    }

    pub fn node_id(&self) -> NodeId {
        delegate_to_inner!(self, node_id)
    }

    pub fn current_term(&self) -> Term {
        delegate_to_inner!(self, current_term)
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        delegate_to_inner!(self, voted_for)
    }

    pub fn commit_index(&self) -> LogIndex {
        delegate_to_inner!(self, commit_index)
    }

    pub fn signal_counter(&self) -> u64 {
        delegate_to_inner!(self, signal_counter)
    }

    pub fn consensus_progress(&self) -> ConsensusProgress {
        ConsensusProgress {
            term: self.current_term(),
            commit_index: self.commit_index(),
            signal_counter: self.signal_counter(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tonic::Status;

    use super::*;
    use crate::fsm::StateMachine;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Status> {
            Ok(())
        }
    }

    fn setup_node(node_id: u64) -> LogicalNode {
        let fsm = Arc::new(MockFsm);
        LogicalNode::Follower(RaftNode::<Follower>::new(NodeId::new(node_id), fsm))
    }

    mod handle_append_entries {
        use super::*;

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let mut state = setup_node(1);
            state.transition(|old| match old {
                LogicalNode::Follower(n) => LogicalNode::Candidate(n.into_candidate()),
                _ => panic!("Setup failed"),
            });

            // AppendEntries from leader of same term
            let result = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1),
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await;

            assert!(result.success);
            assert_eq!(result.term, Term::new(1));
            assert!(matches!(state, LogicalNode::Follower(_)));
        }

        #[tokio::test]
        async fn demotes_any_role_on_higher_term() {
            let mut state = setup_node(1);
            state.transition(|old| match old {
                LogicalNode::Follower(n) => {
                    LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                }
                _ => panic!("Setup failed"),
            });

            let result = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(10), // Higher term
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await;

            assert!(result.success);
            assert_eq!(result.term, Term::new(10));
            assert!(matches!(state, LogicalNode::Follower(_)));
        }

        #[tokio::test]
        async fn rejects_stale_term() {
            let mut state = setup_node(1);
            state.transition(|old| old.into_follower(Term::new(5), None));

            let result = state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1), // Stale term
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await;

            assert!(!result.success);
            assert_eq!(result.term, Term::new(5));
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn halts_on_rival_leader_same_term() {
            let mut state = setup_node(1);
            state.transition(|old| match old {
                LogicalNode::Follower(n) => {
                    LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                }
                _ => panic!("Setup failed"),
            });

            state
                .handle_append_entries(
                    NodeId::new(2),
                    Term::new(1), // Same term as local leader
                    LogIndex::ZERO,
                    Term::ZERO,
                    vec![],
                    LogIndex::ZERO,
                )
                .await;
        }
    }

    mod handle_request_vote {
        use super::*;

        #[test]
        fn demotes_on_higher_term_even_if_vote_rejected() {
            let mut state = setup_node(1);
            state.transition(|old| match old {
                LogicalNode::Follower(mut n) => {
                    n.log_mut().push(LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    });
                    LogicalNode::Candidate(n.into_candidate())
                }
                _ => panic!("Setup failed"),
            });

            // Request from higher term, but with stale candidate log
            let result = state.handle_request_vote(
                NodeId::new(2),
                Term::new(5),
                LogIndex::ZERO, // Stale index
                Term::ZERO,
            );

            assert!(!result.vote_granted);
            assert_eq!(result.term, Term::new(5));
            assert!(matches!(state, LogicalNode::Follower(_)));
        }

        #[test]
        fn grants_vote_on_same_term_if_eligible() {
            let mut state = setup_node(1);
            state.transition(|old| old.into_follower(Term::new(1), None));

            let result =
                state.handle_request_vote(NodeId::new(2), Term::new(1), LogIndex::ZERO, Term::ZERO);

            assert!(result.vote_granted);
            assert_eq!(state.voted_for(), Some(NodeId::new(2)));
        }
    }

    mod propose {
        use super::*;

        #[test]
        fn succeeds_when_leader() {
            let mut state = setup_node(1);
            state.transition(|old| match old {
                LogicalNode::Follower(n) => {
                    LogicalNode::Leader(n.into_candidate().into_leader(vec![]))
                }
                _ => panic!("Setup failed"),
            });

            let result = state.propose(vec![42]);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), LogIndex::new(1));
        }

        #[test]
        fn fails_when_not_leader() {
            let mut state = setup_node(1);
            let result = state.propose(vec![42]);
            assert!(matches!(result, Err(ConsensusError::NotLeader)));
        }
    }

    mod transition_safety {
        use super::*;

        #[test]
        #[should_panic(expected = "Halt Mandate: Node is poisoned")]
        fn panics_on_accessing_poisoned_node() {
            let state = LogicalNode::Poisoned;
            let _ = state.node_id();
        }

        #[test]
        fn remains_poisoned_if_transition_closure_panics() {
            let mut state = setup_node(1);

            // Execute a transition that is guaranteed to panic
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.transition(|_| panic!("Deliberate panic inside transition"));
            }));

            assert!(result.is_err());
            // Verify that the node is now permanently poisoned
            assert!(state.is_poisoned());
        }
    }
}
