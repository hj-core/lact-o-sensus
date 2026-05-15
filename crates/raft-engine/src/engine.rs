use common::proto::v1::raft::LogEntry;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::ConsensusError;
use common::types::errors::NodeError;
use tracing::debug;
use tracing::error;
use tracing::info;

pub use crate::node::Candidate;
pub use crate::node::ConsensusProgress;
pub use crate::node::Follower;
pub use crate::node::Leader;
pub use crate::node::RaftNode;

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

macro_rules! delegate_async_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match $self {
            LogicalNode::Follower(n) => n.$method($($args),*).await,
            LogicalNode::Candidate(n) => n.$method($($args),*).await,
            LogicalNode::Leader(n) => n.$method($($args),*).await,
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
            self.into_follower(req_term, Some(leader_id));
        } else if req_term == current_term {
            match self {
                LogicalNode::Candidate(_) => {
                    info!(
                        "Candidate recognizing leader {} for term {}. Returning to Follower.",
                        leader_id, req_term
                    );
                    self.into_follower(req_term, Some(leader_id));
                }
                LogicalNode::Leader(_) => {
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. Halting \
                         node.",
                        leader_id, req_term
                    );
                    self.apply_fatal(NodeError::Logical(msg));
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
                let res = node
                    .reconcile_log(
                        req_prev_log_index,
                        req_prev_log_term,
                        entries,
                        req_leader_commit,
                    )
                    .await;

                match res {
                    Ok(result) => {
                        if result.success {
                            AppendEntriesResult::success(self.current_term(), result.last_index)
                        } else {
                            AppendEntriesResult::inconsistent(
                                self.current_term(),
                                result.last_index,
                            )
                        }
                    }
                    Err(e) => self.apply_fatal(e),
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
            self.into_follower(req_term, None);
        }

        // 2. Delegate vote granting logic to physical foundation (§5.2, §5.4)
        let vote_granted = match self {
            LogicalNode::Follower(node) => match node.attempt_grant_vote(
                candidate_id,
                req_term,
                req_last_log_index,
                req_last_log_term,
            ) {
                Ok(granted) => granted,
                Err(e) => self.apply_fatal(e),
            },
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
            LogicalNode::Leader(node) => match node.propose(command) {
                Ok(idx) => Ok(idx),
                Err(e) => self.apply_fatal(e),
            },
            LogicalNode::Poisoned => panic!("Halt Mandate: Node is poisoned"),
            _ => Err(ConsensusError::NotLeader),
        }
    }

    /// Consumes the current state and returns a Follower state for the given
    /// term. This is a universal transition mandated by Raft §5.1.
    pub fn into_follower(&mut self, term: Term, leader_id: Option<NodeId>) {
        self.transition(|old| match old {
            LogicalNode::Follower(n) => match n.into_follower(term, leader_id) {
                Ok(new) => LogicalNode::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            LogicalNode::Candidate(n) => match n.into_follower(term, leader_id) {
                Ok(new) => LogicalNode::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            LogicalNode::Leader(n) => match n.into_follower(term, leader_id) {
                Ok(new) => LogicalNode::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            LogicalNode::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        });
    }

    /// Transitions to Candidate role.
    pub fn into_candidate(&mut self) {
        self.transition(|old| match old {
            LogicalNode::Follower(n) => match n.into_candidate() {
                Ok(new) => LogicalNode::Candidate(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            LogicalNode::Candidate(n) => match n.into_restarted_candidate() {
                Ok(new) => LogicalNode::Candidate(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            other => other,
        });
    }

    /// Transitions to Leader role.
    pub fn into_leader(&mut self, peer_ids: Vec<NodeId>) {
        self.transition(|old| match old {
            LogicalNode::Candidate(n) => match n.into_leader(peer_ids) {
                Ok(new) => LogicalNode::Leader(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            other => other,
        });
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

    /// Updates the commit index.
    pub async fn set_commit_index(&mut self, index: LogIndex) {
        match delegate_async_to_inner!(self, set_commit_index, index) {
            Ok(_) => {}
            Err(e) => self.apply_fatal(e),
        }
    }

    // --- Infallible Storage Accessors (ADR 009: Poison-then-Panic) ---

    pub fn current_term(&mut self) -> Term {
        match delegate_to_inner!(self, current_term) {
            Ok(t) => t,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn voted_for(&mut self) -> Option<NodeId> {
        match delegate_to_inner!(self, voted_for) {
            Ok(v) => v,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn last_log_index(&mut self) -> LogIndex {
        match delegate_to_inner!(self, last_log_index) {
            Ok(idx) => idx,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn last_log_term(&mut self) -> Term {
        match delegate_to_inner!(self, last_log_term) {
            Ok(t) => t,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn get_term_at(&mut self, index: LogIndex) -> Term {
        match delegate_to_inner!(self, get_term_at, index) {
            Ok(t) => t,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn read_entries(&mut self, start: LogIndex, end: LogIndex) -> Vec<LogEntry> {
        match delegate_to_inner!(self, read_entries, start, end) {
            Ok(entries) => entries,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn consensus_progress(&mut self) -> ConsensusProgress {
        match self.try_consensus_progress() {
            Ok(p) => p,
            Err(e) => self.apply_fatal(e),
        }
    }

    // --- Technical Helpers for Halt Mandate ---

    /// Transitions state to Poisoned and panics with a standardized clinical
    /// prefix.
    pub(crate) fn apply_fatal(&mut self, err: NodeError) -> ! {
        error!("FATAL: {}", err);
        *self = LogicalNode::Poisoned;
        panic!("Halt Mandate: {}", err);
    }

    /// Static variant for use within ownership-consuming closures.
    pub(crate) fn apply_fatal_static(err: NodeError) -> ! {
        error!("FATAL: {}", err);
        panic!("Halt Mandate: {}", err);
    }

    // --- Read-Only Fallible Accessors (Diagnostic/Progress) ---

    pub fn try_current_term(&self) -> Result<Term, NodeError> {
        delegate_to_inner!(self, current_term)
    }

    pub fn try_last_log_index(&self) -> Result<LogIndex, NodeError> {
        delegate_to_inner!(self, last_log_index)
    }

    pub fn try_last_log_term(&self) -> Result<Term, NodeError> {
        delegate_to_inner!(self, last_log_term)
    }

    pub fn try_consensus_progress(&self) -> Result<ConsensusProgress, NodeError> {
        match self {
            LogicalNode::Poisoned => Ok(ConsensusProgress {
                term: Term::ZERO,
                commit_index: LogIndex::ZERO,
                is_poisoned: true,
                signal_counter: 0,
            }),
            _ => Ok(ConsensusProgress {
                term: self.try_current_term()?,
                commit_index: self.commit_index(),
                is_poisoned: false,
                signal_counter: self.signal_counter(),
            }),
        }
    }

    // --- Infallible In-Memory Accessors ---

    pub fn is_poisoned(&self) -> bool {
        matches!(self, LogicalNode::Poisoned)
    }

    pub fn node_id(&self) -> NodeId {
        delegate_to_inner!(self, node_id)
    }

    pub fn commit_index(&self) -> LogIndex {
        delegate_to_inner!(self, commit_index)
    }

    pub fn signal_counter(&self) -> u64 {
        delegate_to_inner!(self, signal_counter)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use common::raft_api::StateMachine;
    use common::types::errors::FsmError;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> LogIndex {
            LogIndex::ZERO
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            Ok(())
        }
    }

    fn setup_node(node_id: u64) -> LogicalNode {
        let fsm = Arc::new(MockFsm);
        let storage = Box::new(MemoryStorage::new());
        LogicalNode::Follower(RaftNode::<Follower>::new(
            NodeId::new(node_id),
            fsm,
            storage,
        ))
    }

    mod handle_append_entries {
        use super::*;

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let mut state = setup_node(1);
            state.into_candidate();

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
            state.into_candidate();
            state.into_leader(vec![]);

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
            state.into_follower(Term::new(5), None);

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
            state.into_candidate();
            state.into_leader(vec![]);

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
            match &mut state {
                LogicalNode::Follower(n) => {
                    n.storage_mut()
                        .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![])])
                        .unwrap();
                }
                _ => panic!("Setup failed"),
            }
            state.into_candidate();

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
            state.into_follower(Term::new(1), None);

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
            state.into_candidate();
            state.into_leader(vec![]);

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
