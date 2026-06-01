//! The Logical State Machine orchestrator for the Raft consensus engine.
//!
//! This module implements the middle layer of the "Tri-Layer Onion" model
//! (ADR 009), acting as a dispatcher between the physical role-specific nodes
//! and the imperative signaling shell. It manages high-level role transitions
//! (Follower, Candidate, Leader) and enforces the "Halt Mandate" for
//! protocol invariant violations.

use std::mem;
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
use tracing::instrument;

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
    /// The identifier of the current leader if known, or None.
    pub leader_hint: Option<NodeId>,
    /// The highest read epoch confirmed by a majority (§8).
    pub confirmed_read_epoch: u64,
}

/// The Dispatcher Enum (Logical State Machine).
///
/// Orchestrates the physical Raft node according to its current role, managing
/// the transitions between Raft roles (ADR 002).
#[derive(Debug)]
pub enum RoleState<S: StateMachine> {
    Follower(RaftNode<Follower, S>),
    Candidate(RaftNode<Candidate, S>),
    Leader(RaftNode<Leader, S>),
    Poisoned, // ADR 001: Safety barrier during transition failures
}

/// The logical orchestrator of a Raft node, managing its role state,
/// deterministic clock, and randomized timeouts.
#[derive(Debug)]
pub struct LogicalNode<S: StateMachine> {
    state: RoleState<S>,
    current_tick: Tick,
    thresholds: TickThresholds,
    rng: StdRng,
    /// Freeze-Apply Counter (ADR 011): The FSM is frozen for application as
    /// long as this counter is greater than zero. This allows overlapping
    /// tasks (e.g., compaction + replication) to safely manage the freeze
    /// state.
    snapshot_count: u32,
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

macro_rules! delegate_mut_to_inner {
    ($self:ident, $method:ident $(, $args:expr)*) => {
        match &mut $self.state {
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

/// Decision outcomes from an InstallSnapshot request (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAction {
    /// Snapshot is accepted and the node should proceed with restoration.
    Accepted,
    /// Snapshot is rejected due to a stale term (§5.1).
    Rejected,
    /// Snapshot is ignored because it is redundant or older than the current
    /// state (§7).
    Stale,
}

/// Consolidated result of an InstallSnapshot processing attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallSnapshotResult {
    pub term: Term,
    pub action: SnapshotAction,
}

impl InstallSnapshotResult {
    pub fn accepted(term: Term) -> Self {
        Self {
            term,
            action: SnapshotAction::Accepted,
        }
    }

    pub fn rejected(term: Term) -> Self {
        Self {
            term,
            action: SnapshotAction::Rejected,
        }
    }

    pub fn stale(term: Term) -> Self {
        Self {
            term,
            action: SnapshotAction::Stale,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TermCheckResult {
    Valid,
    Stale,
}

// =============================================================================
// Implementation: LogicalNode (High-Level Protocol Orchestrator)
// =============================================================================

impl<S: StateMachine> LogicalNode<S> {
    /// Enforces Raft §5.1 term hygiene and rival leader protection for leader
    /// RPCs.
    ///
    /// Returns `TermCheckResult::Stale` if the incoming request is older than
    /// the current term. Otherwise, updates the current term, demotes if
    /// necessary, updates the leader hint, and returns
    /// `TermCheckResult::Valid`.
    pub(crate) fn validate_leader_rpc_term(
        &mut self,
        req_term: Term,
        leader_id: NodeId,
        rpc_name: &str,
    ) -> TermCheckResult {
        let current_term = self.current_term();

        // 1. Term check (§5.1)
        if req_term < current_term {
            debug!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                remote_leader = %leader_id,
                req_term = %req_term,
                local_term = %current_term,
                "Rejecting {}: Stale Term",
                rpc_name
            );
            return TermCheckResult::Stale;
        }

        // 2. High-level state transitions and demotions (§5.1, §5.2)
        if req_term > current_term {
            info!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                old_term = %current_term,
                new_term = %req_term,
                remote_leader = %leader_id,
                "Role Transition: -> Follower (Term Discovery via {})",
                rpc_name
            );
            self.into_follower(req_term, Some(leader_id));
        } else if req_term == current_term {
            match &mut self.state {
                RoleState::Candidate(_) => {
                    info!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        term = %req_term,
                        remote_leader = %leader_id,
                        "Role Transition: -> Follower (Leader Recognized via {})",
                        rpc_name
                    );
                    self.into_follower(req_term, Some(leader_id));
                }
                RoleState::Leader(_) => {
                    error!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        term = %req_term,
                        rival_leader = %leader_id,
                        "CRITICAL SAFETY VIOLATION: Rival leader detected during {}. Halting node.",
                        rpc_name
                    );
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {} during \
                         {}. Halting node.",
                        leader_id, req_term, rpc_name
                    );
                    self.apply_fatal(NodeError::Protocol(msg));
                }
                RoleState::Follower(node) => {
                    node.state_mut().set_leader_id(Some(leader_id));
                }
                _ => {}
            }
        }

        TermCheckResult::Valid
    }

    /// Creates a new LogicalNode in the Follower role.
    pub fn try_new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<S>,
        log_store: Arc<dyn LogStorage>,
        thresholds: TickThresholds,
        mut rng: StdRng,
    ) -> Result<Self, NodeError> {
        let current_tick = Tick::ZERO;
        let timeout = thresholds.generate_election_timeout(&mut rng);

        let node =
            RaftNode::<Follower, S>::try_new(identity, fsm, log_store, current_tick, timeout)?;

        Ok(Self {
            state: RoleState::Follower(node),
            current_tick,
            thresholds,
            rng,
            snapshot_count: 0,
        })
    }

    pub fn state(&self) -> &RoleState<S> {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn as_follower_mut(&mut self) -> Option<&mut RaftNode<Follower, S>> {
        match &mut self.state {
            RoleState::Follower(node) => Some(node),
            _ => None,
        }
    }

    pub(crate) fn as_candidate_mut(&mut self) -> Option<&mut RaftNode<Candidate, S>> {
        match &mut self.state {
            RoleState::Candidate(node) => Some(node),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_leader(&self) -> Option<&RaftNode<Leader, S>> {
        match &self.state {
            RoleState::Leader(node) => Some(node),
            _ => None,
        }
    }

    pub(crate) fn as_leader_mut(&mut self) -> Option<&mut RaftNode<Leader, S>> {
        match &mut self.state {
            RoleState::Leader(node) => Some(node),
            _ => None,
        }
    }

    /// Processes an InstallSnapshot RPC (§7).
    ///
    /// HALT MANDATE (ADR 009/011):
    /// This method validates the term and coordinates. The actual destructive
    /// state machine restoration is offloaded by the orchestrator shell.
    #[instrument(
        name = "handle_install_snapshot",
        target = "raft::compaction",
        skip_all,
        fields(leader = %leader_id, term = %req_term, index = %last_included_index)
    )]
    pub fn handle_install_snapshot(
        &mut self,
        leader_id: NodeId,
        req_term: Term,
        last_included_index: LogIndex,
    ) -> InstallSnapshotResult {
        let current_term = self.current_term();

        // 1. Term check & Role Transitions (§5.1, §5.2)
        if self.validate_leader_rpc_term(req_term, leader_id, "InstallSnapshot")
            == TermCheckResult::Stale
        {
            return InstallSnapshotResult::rejected(current_term);
        }

        // 2. Horizon Check (§7): Ignore snapshots that don't move the state forward.
        // We use last_applied() as the authoritative logical horizon.
        if last_included_index <= self.last_applied() {
            debug!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                remote_leader = %leader_id,
                req_index = %last_included_index,
                local_applied = %self.last_applied(),
                "Ignoring InstallSnapshot: Redundant/Stale Index"
            );
            return InstallSnapshotResult::stale(current_term);
        }

        self.reset_heartbeat();

        // In the non-blocking handoff, the logical node acknowledges the
        // snapshot metadata. The ConsensusShell is responsible for executing
        // the restoration tombstone protocol.
        InstallSnapshotResult::accepted(self.current_term())
    }

    /// Explicitly advances the logical state machine's horizon after a
    /// snapshot.
    pub fn advance_horizon_after_snapshot(&mut self, index: LogIndex) -> Result<(), NodeError> {
        delegate_mut_to_inner!(self, advance_horizon_after_snapshot, index)
    }

    /// Returns true if a snapshot is currently being generated.
    pub fn is_snapshotting(&self) -> bool {
        self.snapshot_count > 0
    }

    pub fn log_store(&self) -> Arc<dyn LogStorage> {
        match &self.state {
            RoleState::Follower(n) => n.log_store().clone(),
            RoleState::Candidate(n) => n.log_store().clone(),
            RoleState::Leader(n) => n.log_store().clone(),
            RoleState::Poisoned => panic!("Halt Mandate: Node is poisoned"),
        }
    }

    /// Toggles the Freeze-Apply state for snapshot generation.
    pub fn set_snapshotting(&mut self, active: bool) {
        if active {
            match self.snapshot_count.checked_add(1) {
                Some(new_count) => self.snapshot_count = new_count,
                None => self.apply_fatal(NodeError::Protocol(
                    "Structural Invariant Violation: snapshot_count overflow".to_string(),
                )),
            }
        } else {
            match self.snapshot_count.checked_sub(1) {
                Some(new_count) => self.snapshot_count = new_count,
                None => self.apply_fatal(NodeError::Protocol(
                    "Structural Invariant Violation: snapshot_count underflow (unfreeze without \
                     freeze)"
                        .to_string(),
                )),
            }
        }

        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            active,
            snapshot_count = self.snapshot_count,
            "Freeze-Apply state toggled."
        );
    }

    /// Processes an AppendEntries RPC.
    #[instrument(
        name = "handle_append_entries",
        target = "raft::replication",
        skip_all,
        fields(leader = %leader_id, term = %req_term)
    )]
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

        // 1. Term check & Role Transitions (§5.1, §5.2)
        if self.validate_leader_rpc_term(req_term, leader_id, "AppendEntries")
            == TermCheckResult::Stale
        {
            return AppendEntriesResult::stale_term(current_term);
        }

        // 2. Heartbeat reset (§5.2)
        self.reset_heartbeat();

        // 3. Delegation to physical reconciliation (§5.3)
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
    #[instrument(
        name = "handle_request_vote",
        target = "raft::foundation",
        skip_all,
        fields(candidate = %candidate_id, term = %req_term)
    )]
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
    #[instrument(
        name = "propose",
        target = "raft::foundation",
        skip_all,
        fields(command_len = command.len())
    )]
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
    #[instrument(
        name = "transition_to_follower",
        target = "raft::foundation",
        skip_all,
        fields(term = %term, leader = ?leader_id)
    )]
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
    #[instrument(
        name = "transition_to_candidate",
        target = "raft::foundation",
        skip_all
    )]
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
    #[instrument(name = "transition_to_leader", target = "raft::foundation", skip_all)]
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
        F: FnOnce(RoleState<S>) -> RoleState<S>,
    {
        let old_state = mem::replace(&mut self.state, RoleState::Poisoned);
        self.state = f(old_state);
    }

    /// Advances the monotonic system clock and evaluates the current role state
    /// for deterministic timeout actions.
    pub fn tick(&mut self) -> TickAction {
        self.current_tick.increment();
        let now = self.current_tick;

        match &mut self.state {
            RoleState::Follower(node) => node.state().evaluate_tick(now),
            RoleState::Candidate(node) => node.state().evaluate_tick(now),
            RoleState::Leader(node) => node
                .state()
                .evaluate_tick(now, self.thresholds.heartbeat_interval),
            RoleState::Poisoned => TickAction::Stop,
        }
    }

    /// Resets the election or heartbeat timer based on the current role.
    pub fn reset_heartbeat(&mut self) {
        let tick = self.current_tick;
        match &mut self.state {
            RoleState::Follower(node) => node.state_mut().reset_heartbeat(tick),
            RoleState::Leader(node) => node.state_mut().reset_heartbeat(tick),
            _ => {}
        }
    }

    /// Updates the commit index.
    ///
    /// FREEZE-APPLY (ADR 011):
    /// If a snapshot is in progress, this method only advances the logical
    /// commit index without applying entries to the FSM.
    pub async fn advance_last_committed(&mut self, index: LogIndex) {
        if self.is_snapshotting() {
            self.update_commit_index_only(index);
            return;
        }

        match delegate_async_to_inner!(self, advance_last_committed, index) {
            Ok(_) => {}
            Err(e) => self.apply_fatal(e),
        }
    }

    /// Updates the commit index without triggering FSM application.
    pub fn update_commit_index_only(&mut self, index: LogIndex) {
        match delegate_mut_to_inner!(self, update_commit_index_only, index) {
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

    pub fn last_included_index(&mut self) -> LogIndex {
        match delegate_to_inner!(self, last_included_index) {
            Ok(idx) => idx,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn last_included_term(&mut self) -> Term {
        match delegate_to_inner!(self, last_included_term) {
            Ok(t) => t,
            Err(e) => self.apply_fatal(e),
        }
    }

    pub fn save_snapshot_metadata(&mut self, index: LogIndex, term: Term) {
        match delegate_mut_to_inner!(self, save_snapshot_metadata, index, term) {
            Ok(_) => {}
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
                leader_hint: None,
                confirmed_read_epoch: 0,
            }),
            RoleState::Follower(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Follower,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
                leader_hint: n.state().leader_id(),
                confirmed_read_epoch: 0,
            }),
            RoleState::Candidate(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Candidate,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
                leader_hint: None,
                confirmed_read_epoch: 0,
            }),
            RoleState::Leader(n) => Ok(ConsensusProgress {
                term: n.current_term()?,
                role: NodeRole::Leader,
                last_log_index: n.last_log_index()?,
                last_committed: n.last_committed(),
                last_applied: n.last_applied(),
                leader_hint: Some(self.identity().node_id()),
                confirmed_read_epoch: n.state().confirmed_read_epoch(),
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

    pub fn fsm(&self) -> Arc<S> {
        delegate_to_inner!(self, fsm)
    }

    /// Returns the high-level role of the logical node.
    pub fn node_role(&self) -> NodeRole {
        match &self.state {
            RoleState::Follower(_) => NodeRole::Follower,
            RoleState::Candidate(_) => NodeRole::Candidate,
            RoleState::Leader(_) => NodeRole::Leader,
            RoleState::Poisoned => NodeRole::Poisoned,
        }
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
    use common::types::trace::TraceId;
    use rand::SeedableRng;

    use super::*;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied: u64,
    }
    #[async_trait]
    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::new(self.applied))
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        async fn install_snapshot(
            &self,
            _last_included_index: LogIndex,
            _data: &[u8],
            _trace_id: TraceId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(id).unwrap(),
        ))
    }

    fn setup_node(node_id: u64) -> LogicalNode<MockFsm> {
        let fsm = Arc::new(MockFsm::default());
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = StdRng::seed_from_u64(node_id);
        LogicalNode::try_new(test_identity(node_id), fsm, storage, thresholds, rng).unwrap()
    }

    mod handle_install_snapshot {
        use super::*;

        mod term_safety {
            use super::*;

            #[tokio::test]
            async fn rejects_stale_term() {
                let mut node = setup_node(1);
                node.into_follower(Term::new(5), None);

                let result = node.handle_install_snapshot(
                    NodeId::try_new(2).unwrap(),
                    Term::new(1),
                    LogIndex::new(10),
                );

                assert_eq!(result, InstallSnapshotResult::rejected(Term::new(5)));
                assert_eq!(node.current_term(), Term::new(5));
            }

            #[tokio::test]
            async fn demotes_on_higher_term() {
                let mut node = setup_node(1);
                node.into_candidate();
                node.into_leader(vec![]);

                let result = node.handle_install_snapshot(
                    NodeId::try_new(2).unwrap(),
                    Term::new(10),
                    LogIndex::new(100),
                );

                assert_eq!(result, InstallSnapshotResult::accepted(Term::new(10)));
                assert_eq!(node.current_term(), Term::new(10));
                assert!(matches!(node.state, RoleState::Follower(_)));
            }
        }

        mod horizon_safety {
            use super::*;

            #[tokio::test]
            async fn rejects_stale_snapshot_index() {
                let id = test_identity(1);
                let storage = Arc::new(MemoryStorage::new());
                // 1. Establish persistent horizon at 50
                storage
                    .save_snapshot_metadata(LogIndex::new(50), Term::new(1))
                    .unwrap();
                storage.save_last_committed(LogIndex::new(50)).unwrap();

                // 2. State machine also at 50
                let fsm = Arc::new(MockFsm { applied: 50 });

                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);

                // 3. Initialize node (recovers to 50)
                let mut node = LogicalNode::try_new(id, fsm, storage, thresholds, rng).unwrap();

                // 4. Verification: Snapshot for index 40 is stale
                let result = node.handle_install_snapshot(
                    NodeId::try_new(2).unwrap(),
                    Term::new(1),
                    LogIndex::new(40),
                );

                // Note: The node starts at Term 0, so the returned current_term in the stale
                // result is 0
                assert_eq!(result, InstallSnapshotResult::stale(Term::ZERO));
            }
        }

        mod rival_leader_protection {
            use super::*;

            #[tokio::test]
            #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
            async fn halts_on_rival_leader_same_term() {
                let mut node = setup_node(1);
                node.into_candidate();
                node.into_leader(vec![]);

                node.handle_install_snapshot(
                    NodeId::try_new(2).unwrap(),
                    Term::new(1),
                    LogIndex::new(50),
                );
            }
        }
    }

    mod handle_append_entries {
        use super::*;

        mod term_safety {
            use super::*;

            mod with_follower {
                use super::*;

                #[tokio::test]
                async fn rejects_stale_term() {
                    let mut state = setup_node(1);
                    state.into_follower(Term::new(5), None);

                    let result = state
                        .handle_append_entries(
                            NodeId::try_new(2).unwrap(),
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

            mod with_candidate {
                use super::*;

                #[tokio::test]
                async fn demotes_candidate_on_equal_term() {
                    let mut state = setup_node(1);
                    state.into_candidate();

                    // AppendEntries from leader of same term
                    let result = state
                        .handle_append_entries(
                            NodeId::try_new(2).unwrap(),
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
            }

            mod with_any_role {
                use super::*;

                #[tokio::test]
                async fn demotes_any_role_on_higher_term() {
                    let mut state = setup_node(1);
                    state.into_candidate();
                    state.into_leader(vec![]);

                    let result = state
                        .handle_append_entries(
                            NodeId::try_new(2).unwrap(),
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
            }
        }

        mod rival_leader_protection {
            use super::*;

            mod with_active_leader {
                use super::*;

                #[tokio::test]
                #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
                async fn halts_on_rival_leader_same_term() {
                    let mut state = setup_node(1);
                    state.into_candidate();
                    state.into_leader(vec![]);

                    state
                        .handle_append_entries(
                            NodeId::try_new(2).unwrap(),
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
    }

    mod handle_request_vote {
        use super::*;

        mod term_safety {
            use super::*;

            mod with_candidate {
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
                        NodeId::try_new(2).unwrap(),
                        Term::new(5),
                        LogIndex::ZERO, // Stale index
                        Term::ZERO,
                    );

                    assert!(!result.vote_granted);
                    assert_eq!(result.term, Term::new(5));
                    assert!(matches!(state.state, RoleState::Follower(_)));
                }
            }
        }

        mod vote_granting {
            use super::*;

            mod with_follower {
                use super::*;

                #[test]
                fn grants_vote_on_same_term_if_eligible() {
                    let mut state = setup_node(1);
                    state.into_follower(Term::new(1), None);

                    let result = state.handle_request_vote(
                        NodeId::try_new(2).unwrap(),
                        Term::new(1),
                        LogIndex::ZERO,
                        Term::ZERO,
                    );

                    assert!(result.vote_granted);
                    assert_eq!(state.voted_for(), Some(NodeId::try_new(2).unwrap()));
                }
            }
        }
    }

    mod propose {
        use super::*;

        mod role_restrictions {
            use super::*;

            mod with_leader {
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
            }

            mod with_follower {
                use super::*;

                #[test]
                fn fails_when_not_leader() {
                    let mut state = setup_node(1);
                    let result = state.propose(vec![42]);
                    assert!(matches!(result, Err(NodeError::NotLeader { .. })));
                }
            }
        }
    }

    mod consensus_progress {
        use super::*;

        mod state_reporting {
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
            fn reports_confirmed_epoch() {
                let mut state = setup_node(1);
                state.into_candidate();
                state.into_leader(vec![NodeId::try_new(2).unwrap()]);

                // Simulate epoch advancement in physical node
                if let RoleState::Leader(ref mut n) = state.state {
                    n.state_mut()
                        .prepare_read_probe(NodeId::try_new(1).unwrap());
                    n.state_mut()
                        .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
                }

                let progress = state.consensus_progress();
                assert_eq!(progress.confirmed_read_epoch, 1);

                // Start new round
                if let RoleState::Leader(ref mut n) = state.state {
                    n.state_mut()
                        .prepare_read_probe(NodeId::try_new(1).unwrap());
                    n.state_mut()
                        .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
                }

                let progress2 = state.consensus_progress();
                assert_eq!(progress2.confirmed_read_epoch, 2);
            }

            #[test]
            fn reports_poisoned_state_accurately() {
                let fsm = Arc::new(MockFsm::default());
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
    }

    mod transition_safety {
        use super::*;

        mod with_poisoned_node {
            use super::*;

            #[test]
            #[should_panic(expected = "Halt Mandate: Node is poisoned")]
            fn panics_on_propose_when_poisoned() {
                let fsm = Arc::new(MockFsm::default());
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
                let fsm = Arc::new(MockFsm::default());
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
        }

        mod during_transition {
            use super::*;

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

    mod tick {
        use super::*;

        mod with_follower {
            use super::*;

            #[test]
            fn should_not_trigger_election_before_timeout() {
                let mut node = setup_node(1);
                // Min election is 15 in setup_node
                for _ in 0..14 {
                    assert_eq!(node.tick(), TickAction::None);
                }
            }

            #[test]
            fn should_trigger_election_at_timeout() {
                let mut node = setup_node(1);
                // We don't know the exact randomized timeout (15..30),
                // but it MUST trigger by 30.
                let mut triggered = false;
                for _ in 0..30 {
                    if node.tick() == TickAction::StartElection {
                        triggered = true;
                        break;
                    }
                }
                assert!(triggered);
            }

            #[test]
            fn should_reset_timer_on_heartbeat() {
                let mut node = setup_node(1);
                // Advance almost to timeout
                for _ in 0..10 {
                    node.tick();
                }

                node.reset_heartbeat();

                // Should now survive another 10 ticks without triggering
                for _ in 0..10 {
                    assert_eq!(node.tick(), TickAction::None);
                }
            }
        }

        mod with_candidate {
            use super::*;

            #[test]
            fn should_trigger_election_restart_on_timeout() {
                let mut node = setup_node(1);
                node.into_candidate();

                let mut triggered = false;
                for _ in 0..30 {
                    if node.tick() == TickAction::StartElection {
                        triggered = true;
                        break;
                    }
                }
                assert!(triggered);
            }
        }

        mod with_leader {
            use super::*;

            #[test]
            fn should_trigger_heartbeat_at_interval() {
                let mut node = setup_node(1);
                node.into_candidate();
                node.into_leader(vec![]);

                // Heartbeat interval is 10 in setup_node
                for _ in 0..9 {
                    assert_eq!(node.tick(), TickAction::None);
                }
                assert_eq!(node.tick(), TickAction::SendHeartbeat);
            }

            #[test]
            fn should_reset_heartbeat_timer_on_manual_reset() {
                let mut node = setup_node(1);
                node.into_candidate();
                node.into_leader(vec![]);

                for _ in 0..5 {
                    node.tick();
                }
                node.reset_heartbeat();

                // Should now need 10 more ticks
                for _ in 0..9 {
                    assert_eq!(node.tick(), TickAction::None);
                }
                assert_eq!(node.tick(), TickAction::SendHeartbeat);
            }
        }

        mod with_poisoned_node {
            use super::*;

            #[test]
            fn should_return_stop_action() {
                let mut node = setup_node(1);
                node.poison();
                assert_eq!(node.tick(), TickAction::Stop);
            }
        }
    }

    mod snapshot_safety {
        use super::*;

        mod overlapping_tasks {
            use super::*;

            #[tokio::test]
            async fn remains_frozen_until_all_tasks_finish() {
                let mut node = setup_node(1);
                assert!(!node.is_snapshotting());

                // Task 1 starts
                node.set_snapshotting(true);
                assert!(node.is_snapshotting());

                // Task 2 starts
                node.set_snapshotting(true);
                assert!(node.is_snapshotting());

                // Task 1 finishes
                node.set_snapshotting(false);

                // With the reference-counted implementation, the node SHOULD
                // remain in the snapshotting state because Task 2 is still active.
                assert!(
                    node.is_snapshotting(),
                    "FSM unfrozen while Task 2 is still active!"
                );

                // Task 2 finishes
                node.set_snapshotting(false);
                assert!(!node.is_snapshotting());
            }
        }
    }
}
