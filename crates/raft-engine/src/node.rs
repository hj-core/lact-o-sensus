use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::NodeError;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::storage::LogStorage;

// =============================================================================
// 1. Public Snapshots & Types
// =============================================================================

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

// =============================================================================
// 2. Role Markers (Type-State Engine)
// =============================================================================

#[derive(Debug)]
pub struct Follower {
    leader_id: Option<NodeId>,
    last_heartbeat: Instant,
}

#[derive(Debug, Default)]
pub struct Candidate {
    votes_received: HashSet<NodeId>,
}

#[derive(Debug, Default)]
pub struct Leader {
    next_index: HashMap<NodeId, LogIndex>,
    match_index: HashMap<NodeId, LogIndex>,
}

pub trait NodeState: Debug {}
impl NodeState for Follower {}
impl NodeState for Candidate {}
impl NodeState for Leader {}

// =============================================================================
// 3. Shared Shared Container (RaftNode)
// =============================================================================

/// Container for Raft state that is shared across all roles or must persist.
///
/// Represents the "Physical Node" layer, managing log log_store, term
/// persistence, and the commitment boundary.
///
/// SILENT STATE MACHINE: This struct is a pure data mutator. It does NOT own
/// signaling channels or perform I/O. Signaling is the responsibility of the
/// high-level orchestrator shell.
#[derive(Debug)]
pub struct RaftNode<S: NodeState> {
    identity: Arc<NodeIdentity>,
    fsm: Arc<dyn StateMachine>,
    log_store: Arc<dyn LogStorage>,

    // --- Volatile State ---
    last_committed: LogIndex,
    last_applied: LogIndex,
    state: S,
}

// --- Implementation: Shared Accessors ---

impl<S: NodeState> RaftNode<S> {
    pub fn cluster_id(&self) -> &ClusterId {
        self.identity.cluster_id()
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    pub fn identity(&self) -> Arc<NodeIdentity> {
        self.identity.clone()
    }

    pub fn current_term(&self) -> Result<Term, NodeError> {
        self.log_store.current_term().map_err(NodeError::from)
    }

    pub fn voted_for(&self) -> Result<Option<NodeId>, NodeError> {
        self.log_store.voted_for().map_err(NodeError::from)
    }

    pub fn last_committed(&self) -> LogIndex {
        self.last_committed
    }

    pub fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    // --- Log Queries ---

    pub fn last_log_index(&self) -> Result<LogIndex, NodeError> {
        self.log_store.last_log_index().map_err(NodeError::from)
    }

    pub fn last_log_term(&self) -> Result<Term, NodeError> {
        self.log_store.last_log_term().map_err(NodeError::from)
    }

    pub fn get_term_at(&self, index: LogIndex) -> Result<Term, NodeError> {
        if index == LogIndex::ZERO {
            return Ok(Term::ZERO);
        }
        self.log_store
            .read_entry(index)
            .map_err(NodeError::from)
            .map(|opt| opt.map(|e| Term::new(e.term)).unwrap_or(Term::ZERO))
    }

    pub(crate) fn read_entries(
        &self,
        start: LogIndex,
        end: LogIndex,
    ) -> Result<Vec<LogEntry>, NodeError> {
        self.log_store
            .read_entries(start, end)
            .map_err(NodeError::from)
    }

    #[cfg(test)]
    pub(crate) fn log_store(&self) -> &dyn LogStorage {
        self.log_store.as_ref()
    }
}

// --- Implementation: Shared Physical Mutations ---

impl<S: NodeState> RaftNode<S> {
    /// Consumes the current node and returns it in a Follower role.
    /// This is the primary mechanism for demotion and term updates.
    ///
    /// NOTE: This is a pure factory transformation.
    pub fn try_into_follower(
        self,
        term: Term,
        leader_id: Option<NodeId>,
    ) -> Result<RaftNode<Follower>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Follower::new(leader_id),
        };

        // Transition to next term if higher (§5.1)
        node.advance_term(term)?;
        Ok(node)
    }

    /// Updates the commit index and triggers the application of entries to the
    /// FSM.
    pub async fn advance_last_committed(&mut self, index: LogIndex) -> Result<(), NodeError> {
        if index < self.last_committed {
            debug!(
                "Ignoring stale last_committed update: {} < current {}",
                index, self.last_committed
            );
            return Ok(());
        }

        let last_idx = self.last_log_index()?;
        if index > last_idx {
            return Err(NodeError::Logical(format!(
                "Attempted to commit index {} but last_log_index is {}",
                index, last_idx
            )));
        }

        if index > self.last_committed {
            // Persist last_committed BEFORE applying to FSM to ensure safety
            // across crashes.
            self.log_store
                .save_last_committed(index)
                .map_err(NodeError::from)?;

            self.last_committed = index;
            self.apply_to_state_machine().await?;
        }
        Ok(())
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    async fn apply_to_state_machine(&mut self) -> Result<(), NodeError> {
        // Safety Barrier: Ensure FSM hasn't regressed or moved ahead of log.
        let fsm_last = self.fsm.last_applied_index().map_err(NodeError::from)?;
        if fsm_last > self.last_committed {
            return Err(NodeError::Logical(format!(
                "FSM index {} is ahead of last_committed {}. Possible log regression.",
                fsm_last, self.last_committed
            )));
        }

        while self.last_applied < self.last_committed {
            let apply_idx = self.last_applied + 1;
            let entry = self.log_store.read_entry(apply_idx)?.ok_or_else(|| {
                NodeError::Logical(format!(
                    "Committed entry {} missing from log during apply",
                    apply_idx
                ))
            })?;

            if let Err(e) = self.fsm.apply(apply_idx, &entry.data).await {
                error!(
                    "State machine failed to apply index {}: {}. Triggering Halt Mandate.",
                    apply_idx, e
                );
                return Err(NodeError::from(e));
            }

            self.last_applied = apply_idx;
        }
        Ok(())
    }

    /// Updates the current term and resets voting state if the term increased.
    pub(crate) fn advance_term(&mut self, term: Term) -> Result<(), NodeError> {
        let current = self.log_store.current_term()?;
        if term < current {
            return Err(NodeError::Logical(format!(
                "Term regression detected! current={} new={}",
                current, term
            )));
        }
        if term > current {
            self.log_store
                .save_hard_state(term, None)
                .map_err(NodeError::from)?;
        }
        Ok(())
    }

    pub fn vote_for(&mut self, candidate_id: NodeId) -> Result<(), NodeError> {
        let term = self.log_store.current_term()?;
        self.log_store
            .save_hard_state(term, Some(candidate_id))
            .map_err(NodeError::from)?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn into_parts(
        self,
    ) -> (
        Arc<NodeIdentity>,
        Arc<dyn StateMachine>,
        Arc<dyn LogStorage>,
        LogIndex,
        LogIndex,
    ) {
        (
            self.identity,
            self.fsm,
            self.log_store,
            self.last_committed,
            self.last_applied,
        )
    }
}

// =============================================================================
// 4. Role: Follower Behavior
// =============================================================================

impl RaftNode<Follower> {
    pub fn new(
        identity: Arc<NodeIdentity>,
        fsm: Arc<dyn StateMachine>,
        log_store: Arc<dyn LogStorage>,
    ) -> Self {
        let last_applied = fsm.last_applied_index().expect("FSM corruption at startup");
        let last_committed = log_store
            .last_committed()
            .expect("Log log_store corruption at startup");

        Self {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Follower::new(None),
        }
    }

    /// Following Raft §5.3, reconciles the local log with entries from the
    /// leader.
    pub async fn reconcile_log(
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

        self.reconcile_last_committed(leader_commit).await?;
        Ok(ReconciliationResult::success(self.last_log_index()?))
    }

    /// Follower -> Candidate transition (Triggered by Election Timeout).
    pub fn try_into_candidate(self) -> Result<RaftNode<Candidate>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Candidate::new(),
        };

        let new_term = node.current_term()? + 1;
        node.advance_term(new_term)?;
        let node_id = node.node_id();
        node.vote_for(node_id)?;
        node.state_mut().add_vote(node_id);
        Ok(node)
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
            self.vote_for(candidate_id)?;
            return Ok(true);
        }
        Ok(false)
    }

    // --- Follower Helpers ---

    fn verify_log_consistency(
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
                "Rejecting AppendEntries: prevLogIndex {} is beyond local log length {}",
                prev_log_index, last_idx
            );
            return Ok(false);
        }

        let local_term = self.get_term_at(prev_log_index)?;
        if local_term != prev_log_term {
            debug!(
                "Rejecting AppendEntries: prevLogIndex {} has term mismatch (local {}, remote {})",
                prev_log_index, local_term, prev_log_term
            );
            return Ok(false);
        }

        Ok(true)
    }

    fn append_entries_with_reconciliation(
        &mut self,
        entries: Vec<LogEntry>,
    ) -> Result<bool, NodeError> {
        for entry in &entries {
            let entry_index = LogIndex::new(entry.index);
            let local_term = self.get_term_at(entry_index)?;
            if local_term != Term::ZERO && local_term != Term::new(entry.term) {
                info!(
                    "Log conflict detected at index {}. Truncating log.",
                    entry_index
                );
                self.log_store
                    .truncate_log(entry_index)
                    .map_err(NodeError::from)?;
                break;
            }
        }

        let mut to_append = Vec::new();
        let mut next_expected = self.last_log_index()? + 1;

        for entry in entries {
            let entry_index = LogIndex::new(entry.index);
            if entry_index >= next_expected {
                if entry_index != next_expected {
                    error!(
                        "Non-contiguous log append attempted by Leader. index={}, expected={}",
                        entry_index, next_expected
                    );
                    return Ok(false);
                }
                to_append.push(entry);
                next_expected = next_expected + 1;
            }
        }

        if !to_append.is_empty() {
            self.log_store
                .append_entries(to_append)
                .map_err(NodeError::from)?;
        }

        Ok(true)
    }

    async fn reconcile_last_committed(&mut self, leader_commit: LogIndex) -> Result<(), NodeError> {
        if leader_commit > self.last_committed {
            let last_new_idx = self.last_log_index()?;
            let new_commit = std::cmp::min(leader_commit, last_new_idx);
            self.advance_last_committed(new_commit).await?;
            debug!("Updated last_committed to {}", new_commit);
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

// =============================================================================
// 5. Role: Candidate Behavior
// =============================================================================

impl RaftNode<Candidate> {
    pub fn try_into_restarted_candidate(self) -> Result<RaftNode<Candidate>, NodeError> {
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let mut node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Candidate::new(),
        };

        let new_term = node.current_term()? + 1;
        node.advance_term(new_term)?;
        let node_id = node.node_id();
        node.vote_for(node_id)?;
        node.state_mut().add_vote(node_id);
        Ok(node)
    }

    pub fn try_into_leader(self, peer_ids: Vec<NodeId>) -> Result<RaftNode<Leader>, NodeError> {
        let last_log_index = self.last_log_index()?;
        let (identity, fsm, log_store, last_committed, last_applied) = self.into_parts();

        let node = RaftNode {
            identity,
            fsm,
            log_store,
            last_committed,
            last_applied,
            state: Leader::new(peer_ids, last_log_index),
        };
        Ok(node)
    }
}

// =============================================================================
// 6. Role: Leader Behavior
// =============================================================================

impl RaftNode<Leader> {
    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, NodeError> {
        let index = self.last_log_index()? + 1;
        let entry = LogEntry::new(index, self.current_term()?, command);
        self.log_store
            .append_entries(vec![entry])
            .map_err(NodeError::from)?;
        Ok(index)
    }
}

// =============================================================================
// 7. Role Marker Boilerplate
// =============================================================================

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

    pub fn set_leader_id(&mut self, leader_id: Option<NodeId>) {
        self.leader_id = leader_id;
    }

    pub fn reset_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }
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

impl Leader {
    pub fn new(peer_ids: Vec<NodeId>, last_log_index: LogIndex) -> Self {
        let mut next_index = HashMap::new();
        let mut match_index = HashMap::new();

        for peer_id in peer_ids {
            next_index.insert(peer_id, last_log_index + 1);
            match_index.insert(peer_id, LogIndex::new(0));
        }

        Self {
            next_index,
            match_index,
        }
    }

    pub fn next_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.next_index
    }

    pub fn next_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.next_index
    }

    pub fn match_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.match_index
    }

    pub fn match_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.match_index
    }
}

// =============================================================================
// 8. Behavioral Specification (Tests)
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use common::types::errors::FsmError;

    use super::*;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm {
        applied_indices: Mutex<Vec<LogIndex>>,
        applied_data: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), FsmError> {
            self.applied_indices.lock().unwrap().push(index);
            self.applied_data.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(id),
        ))
    }

    mod shared_primitives {
        use super::*;

        #[test]
        fn advance_term_returns_error_on_regression() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);
            node.advance_term(Term::new(10)).unwrap();
            let result = node.advance_term(Term::new(5));
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("regression"));
        }

        #[tokio::test]
        async fn advance_last_committed_persists_to_log_store() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![],
                }])
                .unwrap();
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

            node.advance_last_committed(LogIndex::new(1)).await.unwrap();

            assert_eq!(node.log_store.last_committed().unwrap(), LogIndex::new(1));
        }

        #[tokio::test]
        async fn apply_to_state_machine_detects_fsm_regression() {
            #[derive(Debug, Default)]
            struct RegressionFsm;
            #[async_trait]
            impl StateMachine for RegressionFsm {
                fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
                    Ok(LogIndex::new(100))
                }

                async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
                    Ok(())
                }
            }

            let mut node = RaftNode::<Follower>::new(
                test_identity(1),
                Arc::new(RegressionFsm),
                Arc::new(MemoryStorage::new()),
            );

            let result = node.apply_to_state_machine().await;
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("ahead of last_committed")
            );
        }

        #[tokio::test]
        async fn advance_last_committed_applies_to_fsm() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![1],
                }])
                .unwrap();
            let mut node =
                RaftNode::<Follower>::new(test_identity(1), fsm.clone(), Arc::new(log_store));

            node.advance_last_committed(LogIndex::new(1)).await.unwrap();

            assert_eq!(node.last_committed(), LogIndex::new(1));
            assert_eq!(fsm.applied_indices.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn advance_last_committed_ignores_stale_index() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![],
                }])
                .unwrap();
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));
            node.advance_last_committed(LogIndex::new(1)).await.unwrap();

            node.advance_last_committed(LogIndex::ZERO).await.unwrap();

            assert_eq!(node.last_committed(), LogIndex::new(1));
        }

        #[tokio::test]
        async fn advance_last_committed_applies_multiple_entries_sequentially() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            let mut entries = Vec::new();
            for i in 1..=3 {
                entries.push(LogEntry {
                    index: i,
                    term: 1,
                    data: vec![i as u8],
                });
            }
            log_store.append_entries(entries).unwrap();
            let mut node =
                RaftNode::<Follower>::new(test_identity(1), fsm.clone(), Arc::new(log_store));

            node.advance_last_committed(LogIndex::new(3)).await.unwrap();

            let applied = fsm.applied_indices.lock().unwrap();
            assert_eq!(
                applied.as_slice(),
                &[LogIndex::new(1), LogIndex::new(2), LogIndex::new(3)]
            );
            assert_eq!(node.last_applied(), LogIndex::new(3));
        }

        #[tokio::test]
        async fn apply_to_state_machine_returns_error_on_physical_log_corruption() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);
            node.last_committed = LogIndex::new(5);
            let result = node.apply_to_state_machine().await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("missing from log"));
        }

        #[tokio::test]
        async fn advance_last_committed_returns_error_on_boundary_violation() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);
            let result = node.advance_last_committed(LogIndex::new(1)).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("last_log_index"));
        }

        #[tokio::test]
        async fn advance_last_committed_returns_error_when_fsm_fails() {
            #[derive(Debug, Default)]
            struct PoisonFsm;
            #[async_trait]
            impl StateMachine for PoisonFsm {
                fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
                    Ok(LogIndex::ZERO)
                }

                async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
                    Err(FsmError::invariant("Simulated FSM failure"))
                }
            }

            let log_store = MemoryStorage::new();
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![1],
                }])
                .unwrap();
            let mut node = RaftNode::<Follower>::new(
                test_identity(1),
                Arc::new(PoisonFsm),
                Arc::new(log_store),
            );

            let result = node.advance_last_committed(LogIndex::new(1)).await;
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                matches!(err, NodeError::Logical(_)),
                "Expected NodeError::Logical, got {:?}",
                err
            );
            assert!(err.to_string().contains("Simulated FSM failure"));
        }
    }

    mod follower_ops {
        use super::*;

        fn setup_node() -> RaftNode<Follower> {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            RaftNode::<Follower>::new(test_identity(1), fsm, log_store)
        }

        #[tokio::test]
        async fn reconcile_log_rejects_inconsistent_prev_index() {
            let mut node = setup_node();
            let result = node
                .reconcile_log(LogIndex::new(1), Term::new(1), vec![], LogIndex::ZERO)
                .await
                .unwrap();
            assert!(!result.success);
        }

        #[tokio::test]
        async fn reconcile_log_detects_conflicts_and_truncates() {
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
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

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
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.last_index, LogIndex::new(2));
            assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
        }

        #[tokio::test]
        async fn reconcile_log_truncates_conflicting_suffix() {
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
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

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
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.last_index, LogIndex::new(2));
            assert_eq!(node.get_term_at(LogIndex::new(2)).unwrap(), Term::new(2));
            assert_eq!(node.last_log_index().unwrap(), LogIndex::new(2));
        }

        #[tokio::test]
        async fn reconcile_log_is_idempotent_for_duplicate_entries() {
            let fsm = Arc::new(MockFsm::default());
            let entry = LogEntry {
                index: 1,
                term: 1,
                data: vec![1],
            };
            let log_store = MemoryStorage::new();
            log_store.append_entries(vec![entry.clone()]).unwrap();
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

            let result = node
                .reconcile_log(LogIndex::ZERO, Term::ZERO, vec![entry], LogIndex::ZERO)
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
        }

        #[tokio::test]
        async fn reconcile_log_rejects_non_contiguous_append() {
            let mut node = setup_node();
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
                .await
                .unwrap();

            assert!(!result.success);
            assert_eq!(node.last_log_index().unwrap(), LogIndex::ZERO);
        }

        #[test]
        fn grant_vote_respects_voting_state() {
            let mut node = setup_node();
            node.advance_term(Term::new(1)).unwrap();
            node.vote_for(NodeId::new(3)).unwrap();

            let granted = node
                .attempt_grant_vote(NodeId::new(2), Term::new(1), LogIndex::ZERO, Term::ZERO)
                .unwrap();
            assert!(!granted);

            let granted = node
                .attempt_grant_vote(NodeId::new(3), Term::new(1), LogIndex::ZERO, Term::ZERO)
                .unwrap();
            assert!(granted);
        }
    }

    mod up_to_date_rule {
        use super::*;

        fn setup_node_with_log(last_idx: u64, last_term: u64) -> RaftNode<Follower> {
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
            RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store))
        }

        #[test]
        fn rejects_stale_last_term() {
            let node = setup_node_with_log(5, 2);
            assert!(
                !node
                    .is_log_up_to_date(Term::new(1), LogIndex::new(10))
                    .unwrap()
            );
        }

        #[test]
        fn accepts_higher_last_term() {
            let node = setup_node_with_log(5, 2);
            assert!(
                node.is_log_up_to_date(Term::new(3), LogIndex::new(1))
                    .unwrap()
            );
        }

        #[test]
        fn handles_equal_terms_with_index_check() {
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

    mod heartbeat_invariants {
        use super::*;

        #[test]
        fn reset_heartbeat_updates_timer() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);
            let initial_time = node.state().last_heartbeat();

            std::thread::sleep(std::time::Duration::from_millis(1));

            node.state_mut().reset_heartbeat();

            assert!(node.state().last_heartbeat() > initial_time);
        }
    }

    mod transitions {
        use super::*;
        use crate::storage::SledStorage;

        #[test]
        fn recovers_state_from_log_store_on_initialization() {
            let fsm = Arc::new(MockFsm::default());
            let dir = tempfile::tempdir().unwrap();

            {
                let db = sled::open(dir.path()).unwrap();
                let log_store = SledStorage::new(db).unwrap();
                log_store
                    .save_hard_state(Term::new(7), Some(NodeId::new(2)))
                    .unwrap();
            }

            {
                let db = sled::open(dir.path()).unwrap();
                let log_store = Arc::new(SledStorage::new(db).unwrap());
                let node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);

                assert_eq!(node.current_term().unwrap(), Term::new(7));
                assert_eq!(node.voted_for().unwrap(), Some(NodeId::new(2)));
            }
        }

        #[test]
        fn candidate_transition_preserves_invariants() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store);

            let candidate = node.try_into_candidate().unwrap();

            assert_eq!(candidate.current_term().unwrap(), Term::new(1));
            assert_eq!(candidate.voted_for().unwrap(), Some(NodeId::new(1)));
            assert_eq!(candidate.state().vote_count(), 1);
        }

        #[test]
        fn leader_transition_initializes_indices_correctly() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .append_entries(vec![LogEntry {
                    index: 1,
                    term: 1,
                    data: vec![],
                }])
                .unwrap();
            let node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

            let peer_id = NodeId::new(2);
            let leader = node
                .try_into_candidate()
                .unwrap()
                .try_into_leader(vec![peer_id])
                .unwrap();

            let next_idx = leader.state().next_index().get(&peer_id).unwrap();
            assert_eq!(next_idx.value(), 2);

            let match_idx = leader.state().match_index().get(&peer_id).unwrap();
            assert_eq!(match_idx.value(), 0);
        }

        #[test]
        fn demotion_resets_voting_state_on_new_term() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .save_hard_state(Term::new(1), Some(NodeId::new(1)))
                .unwrap();
            let node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

            let demoted = node.try_into_follower(Term::new(2), None).unwrap();

            assert_eq!(demoted.current_term().unwrap(), Term::new(2));
            assert_eq!(demoted.voted_for().unwrap(), None);
        }

        #[test]
        fn demotion_preserves_vote_on_same_term() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = MemoryStorage::new();
            log_store
                .save_hard_state(Term::new(1), Some(NodeId::new(3)))
                .unwrap();
            let node = RaftNode::<Follower>::new(test_identity(1), fsm, Arc::new(log_store));

            let demoted = node.try_into_follower(Term::new(1), None).unwrap();
            assert_eq!(demoted.voted_for().unwrap(), Some(NodeId::new(3)));
        }

        #[test]
        fn try_into_restarted_candidate_increments_term() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store)
                .try_into_candidate()
                .unwrap();

            let restarted = node.try_into_restarted_candidate().unwrap();

            assert_eq!(restarted.current_term().unwrap(), Term::new(2));
            assert_eq!(restarted.voted_for().unwrap(), Some(NodeId::new(1)));
        }
    }

    mod leader_ops {
        use super::*;

        #[test]
        fn propose_appends_to_log() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = RaftNode::<Follower>::new(test_identity(1), fsm, log_store)
                .try_into_candidate()
                .unwrap()
                .try_into_leader(vec![])
                .unwrap();

            node.propose(vec![1]).unwrap();

            assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
        }
    }
}
