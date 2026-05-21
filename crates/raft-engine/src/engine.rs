use std::sync::Arc;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use rand::rngs::StdRng;
use tracing::debug;
use tracing::error;
use tracing::info;

pub use crate::node::Candidate;
pub use crate::node::Follower;
pub use crate::node::Leader;
pub use crate::node::RaftNode;
pub use crate::node::TickAction;
use crate::storage::LogStorage;
use crate::tick::Tick;
use crate::tick::TickThresholds;

/// The logical role of a Raft node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    Follower,
    Candidate,
    Leader,
    Poisoned,
}

/// Snapshot of the node's consensus progress, used for reactive observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusProgress {
    pub term: Term,
    pub role: NodeRole,
    pub last_log_index: LogIndex,
    pub last_committed: LogIndex,
    pub last_applied: LogIndex,
}

/// The Dispatcher Enum (Logical State Machine).
///
/// This is the primary entry point for all consensus operations, managing
/// the transitions between Raft roles (ADR 002).
#[derive(Debug)]
pub enum RoleState {
    Follower(RaftNode<Follower>),
    Candidate(RaftNode<Candidate>),
    Leader(RaftNode<Leader>),
    Poisoned, // ADR 001: Safety barrier during transition failures
}

/// The logical orchestrator of a Raft node, managing its role state,
/// deterministic clock, and randomized timeouts.
#[derive(Debug)]
pub struct LogicalNode {
    state: RoleState,
    current_tick: Tick,
    thresholds: TickThresholds,
    rng: StdRng,
}

macro_rules! delegate_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match &$self.state {
            RoleState::Follower(n) => n.$method($($args),*),
            RoleState::Candidate(n) => n.$method($($args),*),
            RoleState::Leader(n) => n.$method($($args),*),
            RoleState::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        }
    };
}

macro_rules! delegate_async_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match &mut $self.state {
            RoleState::Follower(n) => n.$method($($args),*).await,
            RoleState::Candidate(n) => n.$method($($args),*).await,
            RoleState::Leader(n) => n.$method($($args),*).await,
            RoleState::Poisoned => panic!("Halt Mandate: Node is poisoned"),
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
    /// Creates a new LogicalNode in the Follower role.
    pub fn try_new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<dyn StateMachine>,
        log_store: Arc<dyn LogStorage>,
        thresholds: TickThresholds,
        mut rng: StdRng,
    ) -> Result<Self, NodeError> {
        let current_tick = Tick::ZERO;
        let timeout = thresholds.generate_election_timeout(&mut rng);

        let node = RaftNode::<Follower>::try_new(identity, fsm, log_store, current_tick, timeout)?;

        Ok(Self {
            state: RoleState::Follower(node),
            current_tick,
            thresholds,
            rng,
        })
    }

    pub fn state(&self) -> &RoleState {
        &self.state
    }

    #[allow(dead_code)]
    pub(crate) fn as_follower_mut(&mut self) -> Option<&mut RaftNode<Follower>> {
        match &mut self.state {
            RoleState::Follower(node) => Some(node),
            _ => None,
        }
    }

    pub(crate) fn as_candidate_mut(&mut self) -> Option<&mut RaftNode<Candidate>> {
        match &mut self.state {
            RoleState::Candidate(node) => Some(node),
            _ => None,
        }
    }

    pub(crate) fn as_leader_mut(&mut self) -> Option<&mut RaftNode<Leader>> {
        match &mut self.state {
            RoleState::Leader(node) => Some(node),
            _ => None,
        }
    }

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
                target: "raft::replication",
                remote_leader = %leader_id,
                req_term = %req_term,
                local_term = %current_term,
                "Rejecting AppendEntries: Stale Term"
            );
            return AppendEntriesResult::stale_term(current_term);
        }

        // 2. High-level state transitions and demotions (§5.1, §5.2)
        if req_term > current_term {
            info!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                old_term = %current_term,
                new_term = %req_term,
                remote_leader = %leader_id,
                "Role Transition: -> Follower (Term Discovery)"
            );
            self.into_follower(req_term, Some(leader_id));
        } else if req_term == current_term {
            match &mut self.state {
                RoleState::Candidate(_) => {
                    info!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        term = %req_term,
                        remote_leader = %leader_id,
                        "Role Transition: -> Follower (Leader Recognized)"
                    );
                    self.into_follower(req_term, Some(leader_id));
                }
                RoleState::Leader(_) => {
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. Halting \
                         node.",
                        leader_id, req_term
                    );
                    self.apply_fatal(NodeError::Protocol(msg));
                }
                RoleState::Follower(node) => {
                    node.state_mut().set_leader_id(Some(leader_id));
                }
                _ => {}
            }
        }

        // 3. Delegation to physical reconciliation (§5.3)
        self.reset_heartbeat();

        match &mut self.state {
            RoleState::Follower(node) => {
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
                    Err(e) => {
                        self.apply_fatal(e);
                    }
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
                target: ClinicalTarget::RaftFoundation.as_str(),
                old_term = %current_term,
                new_term = %req_term,
                candidate = %candidate_id,
                "Role Transition: -> Follower (Term Discovery via Vote)"
            );
            self.into_follower(req_term, None);
        }

        // 2. Delegate vote granting logic to physical foundation (§5.2, §5.4)
        let vote_granted = match &mut self.state {
            RoleState::Follower(node) => match node.attempt_grant_vote(
                candidate_id,
                req_term,
                req_last_log_index,
                req_last_log_term,
            ) {
                Ok(granted) => granted,
                Err(e) => {
                    self.apply_fatal(e);
                }
            },
            _ => false,
        };

        if vote_granted {
            info!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                term = %req_term,
                candidate = %candidate_id,
                "Vote Granted"
            );
            RequestVoteResult::granted(self.current_term())
        } else {
            RequestVoteResult::rejected(self.current_term())
        }
    }

    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, NodeError> {
        match &mut self.state {
            RoleState::Leader(node) => match node.propose(command) {
                Ok(idx) => {
                    let term = self.current_term();
                    debug!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        %term,
                        index = %idx,
                        "Proposal Ingested"
                    );
                    Ok(idx)
                }
                Err(e) => {
                    self.apply_fatal(e);
                }
            },
            RoleState::Poisoned => panic!("Halt Mandate: Node is poisoned"),
            _ => Err(NodeError::NotLeader {
                leader_hint: self.voted_for(),
            }),
        }
    }

    /// Consumes the current state and returns a Follower state for the given
    /// term. This is a universal transition mandated by Raft §5.1.
    pub fn into_follower(&mut self, term: Term, leader_id: Option<NodeId>) {
        let timeout = self.thresholds.generate_election_timeout(&mut self.rng);
        let tick = self.current_tick;

        self.transition(|old_role| match old_role {
            RoleState::Follower(n) => match n.try_into_follower(term, leader_id, tick, timeout) {
                Ok(new) => RoleState::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            RoleState::Candidate(n) => match n.try_into_follower(term, leader_id, tick, timeout) {
                Ok(new) => RoleState::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            RoleState::Leader(n) => match n.try_into_follower(term, leader_id, tick, timeout) {
                Ok(new) => RoleState::Follower(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            RoleState::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        });
    }

    /// Transitions to Candidate role.
    pub fn into_candidate(&mut self) {
        let timeout = self.thresholds.generate_election_timeout(&mut self.rng);
        let tick = self.current_tick;

        self.transition(|old_role| match old_role {
            RoleState::Follower(n) => match n.try_into_candidate(tick, timeout) {
                Ok(new) => RoleState::Candidate(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            RoleState::Candidate(n) => match n.try_into_restarted_candidate(tick, timeout) {
                Ok(new) => RoleState::Candidate(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            other => other,
        });
    }

    /// Transitions to Leader role.
    pub fn into_leader(&mut self, peer_ids: Vec<NodeId>) {
        let tick = self.current_tick;

        self.transition(|old_role| match old_role {
            RoleState::Candidate(n) => match n.try_into_leader(peer_ids, tick) {
                Ok(new) => RoleState::Leader(new),
                Err(e) => Self::apply_fatal_static(e),
            },
            other => other,
        });
    }

    /// Safely transitions the node state using an ownership-consuming closure.
    pub fn transition<F>(&mut self, f: F)
    where
        F: FnOnce(RoleState) -> RoleState,
    {
        let old_state = std::mem::replace(&mut self.state, RoleState::Poisoned);
        self.state = f(old_state);
    }

    /// Resets the election timer if the node is a Follower.
    pub fn reset_heartbeat(&mut self) {
        let tick = self.current_tick;
        if let RoleState::Follower(node) = &mut self.state {
            node.state_mut().reset_heartbeat(tick);
        }
    }

    /// Updates the commit index.
    pub async fn advance_last_committed(&mut self, index: LogIndex) {
        match delegate_async_to_inner!(self, advance_last_committed, index) {
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

    /// Forces the node into a poisoned state.
    pub fn poison(&mut self) {
        self.state = RoleState::Poisoned;
    }

    // --- Technical Helpers for Halt Mandate ---

    /// Transitions state to Poisoned and panics with a standardized clinical
    /// prefix.
    pub(crate) fn apply_fatal(&mut self, err: NodeError) -> ! {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %err,
            "FATAL: Halt Mandate Executed"
        );
        self.state = RoleState::Poisoned;
        panic!("Halt Mandate: {}", err);
    }

    /// Static variant for use within ownership-consuming closures.
    pub(crate) fn apply_fatal_static(err: NodeError) -> ! {
        error!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            error = %err,
            "FATAL: Halt Mandate Executed (Static)"
        );
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
        match &self.state {
            RoleState::Poisoned => Ok(ConsensusProgress {
                term: Term::ZERO,
                role: NodeRole::Poisoned,
                last_log_index: LogIndex::ZERO,
                last_committed: LogIndex::ZERO,
                last_applied: LogIndex::ZERO,
            }),
            RoleState::Follower(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Follower,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
            }),
            RoleState::Candidate(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Candidate,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
            }),
            RoleState::Leader(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Leader,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
            }),
        }
    }

    // --- Infallible In-Memory Accessors ---

    pub fn is_poisoned(&self) -> bool {
        matches!(self.state, RoleState::Poisoned)
    }

    pub fn cluster_id(&self) -> &ClusterId {
        delegate_to_inner!(self, cluster_id)
    }

    pub fn node_id(&self) -> NodeId {
        delegate_to_inner!(self, node_id)
    }

    pub fn identity(&self) -> Arc<NodeIdentity> {
        delegate_to_inner!(self, identity)
    }

    pub fn last_committed(&self) -> LogIndex {
        delegate_to_inner!(self, last_committed)
    }

    pub fn last_applied(&self) -> LogIndex {
        delegate_to_inner!(self, last_applied)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use common::raft_api::StateMachine;
    use common::types::errors::FsmError;
    use rand::SeedableRng;

    use super::*;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(id),
        ))
    }

    fn setup_node(node_id: u64) -> LogicalNode {
        let fsm = Arc::new(MockFsm);
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = StdRng::seed_from_u64(node_id);
        LogicalNode::try_new(test_identity(node_id), fsm, storage, thresholds, rng).unwrap()
    }

    mod handle_append_entries {
        use super::*;

        mod term_safety {
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
                assert!(matches!(state.state, RoleState::Follower(_)));
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
                assert!(matches!(state.state, RoleState::Follower(_)));
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
        }

        mod rival_leader_protection {
            use super::*;

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
    }

    mod handle_request_vote {
        use super::*;

        mod term_safety {
            use super::*;

            #[test]
            fn demotes_on_higher_term_even_if_vote_rejected() {
                let mut state = setup_node(1);
                match &mut state.state {
                    RoleState::Follower(n) => {
                        n.log_store()
                            .append_entries(vec![LogEntry::new(
                                LogIndex::new(1),
                                Term::new(1),
                                vec![],
                            )])
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
                assert!(matches!(state.state, RoleState::Follower(_)));
            }
        }

        mod vote_granting {
            use super::*;

            #[test]
            fn grants_vote_on_same_term_if_eligible() {
                let mut state = setup_node(1);
                state.into_follower(Term::new(1), None);

                let result = state.handle_request_vote(
                    NodeId::new(2),
                    Term::new(1),
                    LogIndex::ZERO,
                    Term::ZERO,
                );

                assert!(result.vote_granted);
                assert_eq!(state.voted_for(), Some(NodeId::new(2)));
            }
        }
    }

    mod propose {
        use super::*;

        mod role_restrictions {
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
                assert!(matches!(result, Err(NodeError::NotLeader { .. })));
            }
        }
    }

    mod consensus_progress {
        use super::*;

        #[test]
        fn reports_follower_state_accurately() {
            let mut state = setup_node(1);
            let progress = state.consensus_progress();
            assert_eq!(progress.role, NodeRole::Follower);
            assert_eq!(progress.term, Term::ZERO);
        }

        #[test]
        fn reports_candidate_state_accurately() {
            let mut state = setup_node(1);
            state.into_candidate();
            let progress = state.consensus_progress();
            assert_eq!(progress.role, NodeRole::Candidate);
            assert_eq!(progress.term, Term::new(1));
        }

        #[test]
        fn reports_leader_state_accurately() {
            let mut state = setup_node(1);
            state.into_candidate();
            state.into_leader(vec![]);
            let progress = state.consensus_progress();
            assert_eq!(progress.role, NodeRole::Leader);
            assert_eq!(progress.term, Term::new(1));
        }

        #[test]
        fn reports_poisoned_state_accurately() {
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let mut state =
                LogicalNode::try_new(test_identity(1), fsm, storage, thresholds, rng).unwrap();
            state.state = RoleState::Poisoned;
            let progress = state.consensus_progress();
            assert_eq!(progress.role, NodeRole::Poisoned);
        }
    }

    mod transition_safety {
        use super::*;

        #[test]
        #[should_panic(expected = "Halt Mandate: Node is poisoned")]
        fn panics_on_propose_when_poisoned() {
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let mut state =
                LogicalNode::try_new(test_identity(1), fsm, storage, thresholds, rng).unwrap();
            state.state = RoleState::Poisoned;
            let _ = state.propose(vec![42]);
        }

        #[test]
        #[should_panic(expected = "Halt Mandate: Node is poisoned")]
        fn panics_on_accessing_poisoned_node() {
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let mut state =
                LogicalNode::try_new(test_identity(1), fsm, storage, thresholds, rng).unwrap();
            state.state = RoleState::Poisoned;
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
