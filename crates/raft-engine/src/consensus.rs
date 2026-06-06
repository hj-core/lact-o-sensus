//! Clinical Consensus Orchestration
//!
//! This module implements the Raft consensus state machine transitions,
//! orchestrating the deterministic heartbeat, leader elections, and log
//! replication cycles.
//!
//! It acts as the "Logical Orchestrator" within the internal node architecture
//! (ADR 009). All asynchronous network fanning is decoupled from the strictly
//! deterministic logical clock driven by the Tick Loop (ADR 003). To maintain
//! clinical integrity, operations explicitly map distributed responses to
//! internal state mutations while propagating causal telemetry traces (ADR
//! 010).

use std::cmp;
use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::InstallSnapshotRequest;
use common::proto::v1::raft::InstallSnapshotResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use common_rpc::TraceInterceptor;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::time::sleep;
use tonic::Request;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;

use crate::config::Config;
use crate::engine::LogicalNode;
use crate::engine::RoleState;
use crate::engine::TickAction;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;
use crate::shell::SnapshotPermit;

// =============================================================================
// I. Semantic Vocabulary (Types & Enums)
// =============================================================================

/// Multiplier applied to the base RPC timeout to allow for connection
/// establishment (TCP/TLS/HTTP2 handshakes) on cold starts (Rule [ENG-03]).
const CONNECTION_CUSHION_MULTIPLIER: u32 = 4;

/// Standardized result type for internal Raft RPC operations.
///
/// Distinguishes transient network or protocol errors (Status) from
/// terminal system-level failures (NodeError).
type RpcResult<T> = Result<T, Status>;

/// Standardized result type for consensus orchestration logic.
type ConsensusResult<T> = Result<T, NodeError>;

// --- 1. The Election Cycle ---

/// Consolidated parameters for a RequestVote RPC.
///
/// Bundles Raft coordinates with telemetry context to ensure causal
/// verification during leadership campaigns (ADR 010).
#[derive(Debug, Clone, Copy)]
struct VoteRequestParams {
    term: Term,
    node_id: NodeId,
    last_log_index: LogIndex,
    last_log_term: Term,
    rpc_timeout: Duration,
    trace_id: TraceId,
}

/// DTO for Election Campaign parameters, captured during the atomic tick
/// boundary.
///
/// Ensures the asynchronous campaign task has a consistent snapshot of the
/// node's identity and log coordinates at the moment the election was
/// triggered.
#[derive(Debug, Clone, Copy)]
struct ElectionCampaignParams {
    term: Term,
    node_id: NodeId,
    last_log_index: LogIndex,
    last_log_term: Term,
    trace_id: TraceId,
}

/// Decision outcomes from the vote-tallying process.
///
/// Maps the distributed responses from peers into immediate state
/// transitions for the Candidate.
#[derive(Debug, PartialEq)]
enum VoteAction {
    /// Quorum has been reached and node has transitioned to Leader.
    QuorumReached,
    /// Node has been demoted to Follower due to a higher term (§5.1).
    Demoted,
    /// Election continues; quorum not yet reached.
    Continue,
}

// --- 2. The Replication Cycle ---

/// Forensic snapshot of a replication attempt.
///
/// Encapsulates the peer's response along with the metadata of the intent sent,
/// allowing the Leader to reconcile its next_index and match_index logic
/// deterministically (ADR 002).
#[derive(Debug)]
enum ReplicationOutcome<S: StateMachine> {
    AppendEntries {
        sent_prev_index: LogIndex,
        sent_entries_len: u64,
        response: AppendEntriesResponse,
    },
    InstallSnapshot {
        last_included_index: LogIndex,
        response: InstallSnapshotResponse,
        /// The permit held during snapshot replication (ADR 011).
        /// This ensures the permit is only released AFTER index reconciliation.
        _permit: SnapshotPermit<S>,
    },
}

/// DTO for Replication Round parameters, captured during the atomic tick
/// boundary.
///
/// Encapsulates the global coordinates required to fan-out log entries to all
/// peers without re-locking the global state for parameter collection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplicationRoundParams {
    pub(crate) term: Term,
    pub(crate) node_id: NodeId,
    pub(crate) last_committed: LogIndex,
    pub(crate) trace_id: TraceId,
}

/// High-level orchestration instructions for the log replication cycle.
///
/// Allows the replication orchestrator to handle opportunistic demotions
/// while continuing to process other peer streams.
#[derive(Debug, PartialEq)]
enum ReplicationAction {
    /// Node has been demoted to Follower due to a higher term (§5.1).
    Demoted,
    /// Replication continues for other peers.
    Continue,
}

/// Logical strategy for a single peer replication attempt.
enum ReplicationStrategy {
    /// Follower is within the log horizon; send incremental updates.
    AppendEntries(AppendEntriesRequest),
    /// Follower is behind the horizon; send the full state machine.
    InstallSnapshot {
        last_included_index: LogIndex,
        last_included_term: Term,
    },
}

// =============================================================================
// II. Public Background Task Orchestrators
// =============================================================================

/// Spawns the unified deterministic Tick Loop.
///
/// This is the system's "Heartbeat" (ADR 003). It pulses at a fixed interval,
/// driving the logical engine's absolute clock and dispatching consensus
/// actions (Elections, Heartbeats) based on deterministic tick boundaries.
///
/// ATOMIC HANDOFF PATTERN: State transitions (like into_candidate) are
/// performed immediately within the locked boundary, and resulting DTOs
/// are handed off to async tasks for network execution. This eliminates
/// the "Double-Acquisition" race condition where the node state could
/// change between the tick and the transition.
pub fn spawn_tick_loop<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.raft.tick_interval();
    let span = tracing::Span::current();

    tokio::spawn(
        async move {
            let mut current_session: Option<(tracing::Span, String, Term)> = None;

            loop {
                // 1. Fixed-interval pulse
                sleep(interval).await;

                // Capture parent span for compaction telemetry (ADR 010).
                // On the first iteration current_session is None, so we fall
                // back to the tick loop's outer span.
                let parent_span = current_session
                    .as_ref()
                    .map(|(s, _, _)| s.clone())
                    .unwrap_or_else(tracing::Span::current);

                // 2. Drive the logical engine and capture the required action
                // Perform state transitions (Atomic Handoff) while holding the lock.
                let (action, role_name, term, campaign, replication) = {
                    let mut guard = state.write().await;
                    let action = guard.tick();
                    let role = determine_node_role_name(&guard);
                    let term = guard.current_term();

                    let mut campaign_params = None;
                    let mut replication_params = None;

                    // COMPACTION TRIGGER (ADR 011)
                    if should_compact_log(&mut guard, &config, state.is_frozen()) {
                        // We set the flag immediately within the locked boundary to
                        // prevent the next tick (10ms later) from re-triggering while
                        // the async task is being spawned.
                        if let Err(e) = state.freeze() {
                            guard.apply_fatal(NodeError::Protocol(e.0.to_string()));
                        }

                        let index = guard.last_applied();
                        let term = guard.get_term_at(index);

                        initiate_log_compaction(state.clone(), index, term, &parent_span);
                    }

                    match action {
                        TickAction::StartElection => {
                            let trace_id = TraceId::generate();
                            guard.into_candidate();
                            campaign_params = Some(ElectionCampaignParams {
                                term: guard.current_term(),
                                node_id: guard.node_id(),
                                last_log_index: guard.last_log_index(),
                                last_log_term: guard.last_log_term(),
                                trace_id,
                            });
                        }
                        TickAction::SendHeartbeat => {
                            let trace_id = TraceId::generate();
                            replication_params = Some(ReplicationRoundParams {
                                term,
                                node_id: guard.node_id(),
                                last_committed: guard.last_committed(),
                                trace_id,
                            });
                        }
                        _ => {}
                    }

                    (
                        action,
                        role.to_string(),
                        term,
                        campaign_params,
                        replication_params,
                    )
                };

                // 3. Telemetry: Manage Role Session Spans (ADR 010)
                // We re-create the span if either the Role or the Term changes
                // to ensure causal accuracy and avoid "Causal Ghosting".
                let identity_changed = current_session
                    .as_ref()
                    .map(|(_, r, t)| r != &role_name || t != &term)
                    .unwrap_or(true);

                if identity_changed {
                    let span = info_span!(
                        target: ClinicalTarget::RaftFoundation.as_str(),
                        "role_session",
                        role = %role_name,
                        term = %term
                    );
                    current_session = Some((span, role_name.clone(), term));
                }

                // We do NOT use .enter() here across spawn calls to avoid Context Pollution.
                // Spans are carried via .instrument() on the spawned futures.
                let session_span = current_session
                    .as_ref()
                    .map(|(s, _, _)| s.clone())
                    .unwrap_or_else(tracing::Span::none);

                // 4. Dispatch deterministic actions using the DTOs
                match action {
                    TickAction::StartElection => {
                        if let Some(params) = campaign {
                            start_election_campaign(
                                config.clone(),
                                state.clone(),
                                peer_manager.clone(),
                                params,
                                session_span,
                            );
                        }
                    }
                    TickAction::SendHeartbeat => {
                        if let Some(params) = replication {
                            initiate_replication(
                                config.clone(),
                                state.clone(),
                                peer_manager.clone(),
                                params,
                                session_span,
                            );
                        }
                    }
                    TickAction::Stop => {
                        error!(
                            target: ClinicalTarget::ClinicalFoundation.as_str(),
                            "Tick loop received Stop signal (Node Poisoned). Halting."
                        );
                        return;
                    }
                    TickAction::None => {}
                }
            }
        }
        .instrument(span),
    )
}

/// Spawns a background task that continuously applies committed log entries
/// to the State Machine.
///
/// This replaces the synchronous `fsm.apply()` call that was previously in
/// `advance_last_committed`. By deferring FSM application to a background
/// loop, the consensus write lock is never held across slow I/O, preventing
/// heartbeat starvation.
pub fn spawn_background_applier<S: StateMachine>(state: Arc<ConsensusShell<S>>) {
    tokio::spawn(async move {
        let mut progress_rx = state.subscribe();

        // Initial catch-up: apply any entries committed before we started.
        crate::orchestration::apply_committed(&state).await;

        loop {
            // Wait for the next progress signal.
            if progress_rx.changed().await.is_err() {
                // The sender was dropped — node is shutting down.
                return;
            }

            // During snapshot freeze (compaction, serialization, or install),
            // skip applying. The applier will be woken again when the snapshot
            // completes via the MutationGuard broadcast.
            if state.is_frozen() {
                continue;
            }

            // Apply any pending committed entries outside the consensus lock.
            crate::orchestration::apply_committed(&state).await;
        }
    });
}

/// Determines whether the density of un-snapshotted applied entries exceeds
/// the configured compaction threshold.
///
/// FREEZE-APPLY INVARIANCE (ADR 011):
/// This check triggers based on `last_applied` rather than `last_log_index` to
/// ensure that every snapshot actually advances the logical horizon of the
/// persisted state. This prevents "Snapshot Storms" where a node repeatedly
/// snapshots the same state when application is lagging behind replication.
///
/// Returns true when the number of un-snapshotted applied entries exceeds
/// `snapshot_threshold` and no snapshot is currently in progress.
fn should_compact_log<S: StateMachine>(
    guard: &mut LogicalNode<S>,
    config: &Config,
    is_frozen: bool,
) -> bool {
    if is_frozen {
        return false;
    }

    let applied = guard.last_applied();
    let last_snap = guard.last_included_index();
    let log_length = (applied - last_snap.as_u64())
        .map(|i| i.as_u64())
        .unwrap_or(0);

    log_length > config.raft.snapshot_threshold
}

// =============================================================================
// III. Election Orchestration (The Candidate's World)
// =============================================================================

/// Spawns an asynchronous task to orchestrate an election campaign.
///
/// Establishes the 'election_campaign' telemetry context parented to the
/// current role session, ensuring causal linkage (ADR 010).
fn start_election_campaign<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: ElectionCampaignParams,
    parent_span: tracing::Span,
) {
    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        parent: &parent_span,
        "election_campaign",
        trace_id = %params.trace_id,
        term = %params.term
    );

    let peer_manager_clone = peer_manager.clone();
    let config_clone = config.clone();

    tokio::spawn(
        async move {
            if let Err(e) =
                initiate_election(config_clone, state.clone(), peer_manager_clone, params).await
            {
                error!( error = %e, "Failed to execute election campaign");
                let mut guard = state.write().await;
                guard.apply_fatal(e);
            }
        }
        .instrument(span),
    );
}

/// Orchestrates a Leadership Campaign by soliciting peer votes.
///
/// Acts as the high-level coordinator: it uses the pre-captured parameters
/// to solicit votes concurrently from all peers and processes the asynchronous
/// stream of responses to determine the campaign's success or failure.
#[instrument(
    name = "election_campaign_execution",
    target = "raft::foundation",
    skip_all,
    fields(term = %params.term, trace_id = %params.trace_id)
)]
async fn initiate_election<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: ElectionCampaignParams,
) -> ConsensusResult<()> {
    info!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        last_log_index = %params.last_log_index,
        last_log_term = %params.last_log_term,
        term = %params.term,
        "Starting election campaign."
    );

    // 1. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let mut vote_stream = broadcast_vote_requests(
        config.as_ref(),
        peer_manager.clone(),
        params.term,
        params.node_id,
        params.last_log_index,
        params.last_log_term,
        params.trace_id,
    );

    // 2. Tally votes and handle term updates
    let mut votes_granted = 1; // Start with 1 (self-vote)
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;

    while let Some((peer_id, res)) = vote_stream.next().await {
        match process_vote_response(&state, params.term, &peer_ids, peer_id, res).await? {
            VoteAction::QuorumReached => return Ok(()),
            VoteAction::Demoted => return Ok(()),
            VoteAction::Continue => {
                // Fetch the current vote count from the formal state machine to ensure
                // consistency with the loop's local tally.
                let guard = state.read().await;
                if let RoleState::Candidate(n) = guard.state() {
                    votes_granted = n.state().vote_count();
                }
            }
        }
    }

    // Loop finished without reaching quorum or being demoted.
    let still_candidate = {
        let guard = state.read().await;
        let current = guard.try_current_term()?;
        matches!(guard.state(), RoleState::Candidate(_) if current == params.term)
    };

    if still_candidate {
        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            votes = %votes_granted,
            quorum = %quorum,
            "Election failed: quorum not reached."
        );
    }

    Ok(())
}

/// Triggers an asynchronous log compaction cycle.
///
/// COMPACTION MECHANICS (ADR 011):
/// In implementations with persistent State Machines, the durable database
/// trees serve as the inherent snapshot. This orchestrator updates the
/// logical horizon metadata and truncates physical log entries that have
/// already been applied to the underlying storage. Peer nodes requiring
/// catch-up will trigger on-demand serialization via StateMachine::snapshot().
pub fn initiate_log_compaction<S: StateMachine>(
    state: Arc<ConsensusShell<S>>,
    index: LogIndex,
    term: Term,
    parent_span: &tracing::Span,
) {
    let span = info_span!(
        target: ClinicalTarget::RaftCompaction.as_str(),
        parent: parent_span,
        "compaction_cycle",
    );

    tokio::spawn(
        async move {
            info!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                index = %index,
                "Log compaction started (FSM Frozen)."
            );

            // 1. Capture snapshot target and update metadata (Locked)
            // The Freeze-Apply state is already set by the Tick Loop.
            let log_store = {
                let mut guard = state.write().await;
                guard.save_snapshot_metadata(index, term);
                guard.log_store()
            };

            // 2. Perform heavy truncation (Unlocked)
            // This is offloaded to a background thread to preserve the tick loop.
            let truncation_res =
                tokio::task::spawn_blocking(move || log_store.truncate_log_front(index)).await;

            // 3. Unfreeze and catch up (Locked)
            {
                let mut guard = state.write().await;
                if let Err(e) = state.thaw() {
                    guard.apply_fatal(NodeError::Protocol(e.0.to_string()));
                }

                // Halt Mandate: If physical truncation fails, the node is in a corrupt state
                // and must poison itself.
                match truncation_res {
                    Ok(Err(e)) => {
                        error!(
                            target: ClinicalTarget::RaftCompaction.as_str(),
                            error = %e,
                            "Log truncation failure! Triggering Halt Mandate."
                        );
                        guard.apply_fatal(NodeError::Protocol(format!(
                            "Log truncation failure at index {}: {}",
                            index, e
                        )));
                    }
                    Err(e) => {
                        error!(
                            target: ClinicalTarget::RaftCompaction.as_str(),
                            error = %e,
                            "Log truncation task panicked or failed to join."
                        );
                        guard.apply_fatal(NodeError::Protocol(format!(
                            "Compaction task join failure: {}",
                            e
                        )));
                    }
                    Ok(Ok(_)) => {}
                }
            }

            // ADR 011: Catch up the State Machine by applying any entries
            // committed during the freeze. This is performed outside the
            // consensus lock to preserve heartbeat liveness.
            crate::orchestration::apply_committed(&state).await;

            info!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                index = %index,
                "Log compaction successful."
            );
        }
        .instrument(span),
    );
}

/// Evaluates a single vote response and determines the immediate state
/// transition.
///
/// Responsible for:
/// 1. Term Integrity: Opportunistic demotion if the peer has a higher term
///    (§5.1).
/// 2. Vote Tallying: Adding granted votes to the Candidate's state machine.
/// 3. Victory Transition: Promoting to Leader immediately upon reaching quorum.
#[instrument(
    name = "process_vote_response",
    target = "raft::foundation",
    skip_all,
    fields(peer = %peer_id, term = %term)
)]
async fn process_vote_response<S: StateMachine>(
    state: &ConsensusShell<S>,
    term: Term,
    peer_ids: &[NodeId],
    peer_id: NodeId,
    res: RpcResult<RequestVoteResponse>,
) -> ConsensusResult<VoteAction> {
    let resp = match res {
        Ok(val) => val,
        Err(e) => {
            debug!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                peer = %peer_id,
                error = %e,
                "Failed to get vote from peer"
            );
            return Ok(VoteAction::Continue);
        }
    };

    let mut guard = state.write().await;
    let resp_term = Term::new(resp.term);

    // 1. Term check and opportunistic demotion (§5.1)
    if resp_term > term {
        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            new_term = %resp_term,
            "Found higher term during election. Demoting to Follower."
        );
        guard.into_follower(resp_term, None);
        return Ok(VoteAction::Demoted);
    }

    // 2. Tally vote if granted and we are still campaigning for the same term
    if resp.vote_granted {
        let mut quorum_reached = false;
        let total_nodes = peer_ids.len() + 1;
        let quorum = (total_nodes / 2) + 1;

        let current_term = guard.current_term();
        #[allow(clippy::collapsible_if)]
        if let Some(node) = guard.as_candidate_mut() {
            if current_term == term {
                node.state_mut().add_vote(peer_id);
                if node.state().vote_count() >= quorum {
                    quorum_reached = true;
                }
            }
        }

        if quorum_reached {
            let last_log_index = guard.last_log_index();
            info!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                term = %term,
                last_log_index = %last_log_index,
                "Quorum reached! Transitioning to Leader."
            );
            guard.into_leader(peer_ids.to_vec());
            return Ok(VoteAction::QuorumReached);
        }
    }

    Ok(VoteAction::Continue)
}

// =============================================================================
// IV. Replication Orchestration (The Leader's World)
// =============================================================================

/// Spawns a dedicated task to orchestrate a single log replication round.
///
/// Ensures the 'replication_round' telemetry context is properly established
/// and linked to the leader session span (ADR 010).
pub(crate) fn initiate_replication<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: ReplicationRoundParams,
    parent_span: tracing::Span,
) {
    let span = info_span!(
        target: ClinicalTarget::RaftReplication.as_str(),
        parent: &parent_span,
        "replication_round",
        trace_id = %params.trace_id,
        term = %params.term
    );

    tokio::spawn(
        async move {
            if let Err(e) = replicate_to_peers(config, state.clone(), peer_manager, params).await {
                error!( error = %e, "Failed to replicate to peers");
                let mut guard = state.write().await;
                guard.apply_fatal(e);
            }
        }
        .instrument(span),
    );
}

/// Orchestrates the fan-out of log entries to all known peers.
///
/// Coordinates the concurrent transmission of AppendEntries RPCs and
/// processes the resulting stream. If a higher term is discovered, it
/// terminates the round early to allow for immediate demotion.
#[instrument(
    name = "replication_round_execution",
    target = "raft::replication",
    skip_all,
    fields(term = %params.term, trace_id = %params.trace_id)
)]
async fn replicate_to_peers<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: ReplicationRoundParams,
) -> ConsensusResult<()> {
    // 1. Prepare and send AppendEntries concurrently to all peers.
    let mut response_stream =
        broadcast_append_entries(&config, peer_manager.clone(), state.clone(), params);

    // 2. Process responses as they arrive (Opportunistic demotion & index updates).
    while let Some((peer_id, res)) = response_stream.next().await {
        if process_replication_response(&state, params.term, peer_id, res).await?
            == ReplicationAction::Demoted
        {
            return Ok(());
        }
    }

    Ok(())
}

/// Evaluates a replication response and updates the Leader's internal
/// bookkeeping.
///
/// Responsible for:
/// 1. Term Integrity: Opportunistic demotion if the peer has a higher term
///    (§5.1).
/// 2. Index Reconciliation: Advancing next_index on success or backtracking on
///    log mismatch (§5.3).
/// 3. Quorum Commitment: Advancing the Leader's commit index once a majority is
///    reached (ADR 002).
#[instrument(
    name = "process_replication_response",
    target = "raft::replication",
    skip_all,
    fields(peer = %peer_id, term = %term)
)]
async fn process_replication_response<S: StateMachine>(
    state: &ConsensusShell<S>,
    term: Term,
    peer_id: NodeId,
    res: RpcResult<Option<ReplicationOutcome<S>>>,
) -> ConsensusResult<ReplicationAction> {
    let outcome = match res {
        Ok(Some(val)) => val,
        Ok(None) => return Ok(ReplicationAction::Continue),
        Err(e) => {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                peer = %peer_id,
                error = %e,
                "Replication RPC failed"
            );
            return Ok(ReplicationAction::Continue);
        }
    };

    let mut guard = state.write().await;

    let resp_term = match &outcome {
        ReplicationOutcome::AppendEntries { response, .. } => Term::new(response.term),
        ReplicationOutcome::InstallSnapshot { response, .. } => Term::new(response.term),
    };

    // 1. Term check and opportunistic demotion (§5.1)
    if resp_term > term {
        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            new_term = %resp_term,
            peer = %peer_id,
            "Found higher term from peer. Demoting to Follower."
        );
        guard.into_follower(resp_term, None);
        return Ok(ReplicationAction::Demoted);
    }

    // 2. Process replication success/failure if we are still the leader for this
    //    term
    let mut last_committed_updated = false;
    let current_term = guard.current_term();
    #[allow(clippy::collapsible_if)]
    if let Some(node) = guard.as_leader_mut() {
        if current_term == term {
            // Acknowledge read quorums (§8)
            let total_nodes = node.state().next_index().len() + 1;
            let quorum = (total_nodes / 2) + 1;
            node.state_mut().acknowledge_heartbeat(peer_id, quorum);

            match outcome {
                ReplicationOutcome::AppendEntries {
                    sent_prev_index,
                    sent_entries_len,
                    response,
                } => {
                    if response.success {
                        let new_match = (sent_prev_index + sent_entries_len).map_err(|e| {
                            NodeError::Protocol(format!(
                                "Arithmetic overflow calculating match_index for peer={} (prev={} \
                                 len={}) in term={}: {}",
                                peer_id, sent_prev_index, sent_entries_len, term, e
                            ))
                        })?;
                        let new_next = (new_match + 1).map_err(|e| {
                            NodeError::Protocol(format!(
                                "Arithmetic overflow calculating next_index for peer={} \
                                 (match={}) in term={}: {}",
                                peer_id, new_match, term, e
                            ))
                        })?;

                        let current_match = *node
                            .state()
                            .match_index()
                            .get(&peer_id)
                            .unwrap_or(&LogIndex::ZERO);

                        if new_match > current_match {
                            node.state_mut().next_index_mut().insert(peer_id, new_next);
                            node.state_mut()
                                .match_index_mut()
                                .insert(peer_id, new_match);
                            last_committed_updated = true;
                        }
                    } else {
                        let current_next = *node
                            .state()
                            .next_index()
                            .get(&peer_id)
                            .unwrap_or(&LogIndex::new(1));

                        let last_log_index = LogIndex::new(response.last_log_index);
                        let new_next = if last_log_index > LogIndex::ZERO {
                            cmp::min(
                                current_next,
                                (last_log_index + 1).map_err(|e| {
                                    NodeError::Protocol(format!(
                                        "Arithmetic overflow calculating next_index backoff for \
                                         peer={} (last_log={}) in term={}: {}",
                                        peer_id, last_log_index, term, e
                                    ))
                                })?,
                            )
                        } else {
                            (current_next - 1)
                                .map(|idx| idx.max(LogIndex::new(1)))
                                .map_err(|e| {
                                    NodeError::Protocol(format!(
                                        "Arithmetic underflow calculating next_index backoff for \
                                         peer={} (current_next={}) in term={}: {}",
                                        peer_id, current_next, term, e
                                    ))
                                })?
                        };

                        node.state_mut().next_index_mut().insert(peer_id, new_next);
                        debug!(
                            target: ClinicalTarget::RaftReplication.as_str(),
                            peer = %peer_id,
                            new_next = %new_next,
                            "Peer rejected AppendEntries (log mismatch). Retrying."
                        );
                    }
                }
                ReplicationOutcome::InstallSnapshot {
                    last_included_index,
                    ..
                } => {
                    // Upon successful InstallSnapshot, catch the peer up to the
                    // snapshot horizon.
                    let new_match = last_included_index;
                    let new_next = (new_match + 1).map_err(|e| {
                        NodeError::Protocol(format!(
                            "Arithmetic overflow calculating next_index after snapshot for \
                             peer={} (match={}) in term={}: {}",
                            peer_id, new_match, term, e
                        ))
                    })?;

                    node.state_mut().next_index_mut().insert(peer_id, new_next);
                    node.state_mut()
                        .match_index_mut()
                        .insert(peer_id, new_match);
                    last_committed_updated = true;

                    info!(
                        target: ClinicalTarget::RaftReplication.as_str(),
                        peer = %peer_id,
                        index = %last_included_index,
                        "Peer successfully caught up via InstallSnapshot."
                    );
                }
            }
        }
    }

    // 3. Opportunistically advance commit index if progress was made
    if last_committed_updated {
        update_leader_last_committed(&mut guard);
    }

    Ok(ReplicationAction::Continue)
}

// =============================================================================
// V. Clinical RPC Layer (Network Implementation)
// =============================================================================

/// Broadcasts RequestVote RPCs concurrently to all cluster peers.
fn broadcast_vote_requests(
    config: &Config,
    peer_manager: Arc<PeerManager>,
    term: Term,
    node_id: NodeId,
    last_log_index: LogIndex,
    last_log_term: Term,
    trace_id: TraceId,
) -> FuturesUnordered<impl futures::Future<Output = (NodeId, RpcResult<RequestVoteResponse>)>> {
    let params = VoteRequestParams {
        term,
        node_id,
        last_log_index,
        last_log_term,
        rpc_timeout: config.raft.rpc_timeout(),
        trace_id,
    };

    peer_manager
        .peer_ids()
        .into_iter()
        .map(|peer_id| {
            let pm = peer_manager.clone();
            async move { (peer_id, request_vote_from_peer(pm, peer_id, params).await) }
        })
        .collect()
}

/// Executes a single RequestVote RPC with strict causal verification.
///
/// Injects the current telemetry trace into the gRPC metadata and verifies
/// that the peer reflects the exact trace ID back, guarding against Byzantine
/// correlation failures (ADR 010).
async fn request_vote_from_peer(
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    params: VoteRequestParams,
) -> RpcResult<RequestVoteResponse> {
    let mut client = peer_manager.get_client(peer_id)?;

    let mut request = Request::new(RequestVoteRequest::new(
        params.term,
        params.node_id,
        params.last_log_index,
        params.last_log_term,
    ));
    request.set_timeout(params.rpc_timeout);

    // Explicit Outbound Propagation
    TraceInterceptor::inject_trace_id_into_request(&mut request, params.trace_id)
        .map_err(|e| Status::internal(format!("Telemetry injection failed: {}", e)))?;

    let response = client.request_vote(request).await?;

    // Causal Integrity Verification (ADR 010)
    verify_trace_integrity(&response, params.trace_id, peer_id)?;

    Ok(response.into_inner())
}

/// Broadcasts AppendEntries RPCs concurrently to all cluster peers.
///
/// Acts as a high-level orchestrator for the replication fan-out, delegating
/// the per-peer request preparation and network handling to
/// `prepare_and_replicate_to_peer`.
fn broadcast_append_entries<S: StateMachine>(
    config: &Config,
    peer_manager: Arc<PeerManager>,
    state: Arc<ConsensusShell<S>>,
    params: ReplicationRoundParams,
) -> FuturesUnordered<
    impl futures::Future<Output = (NodeId, RpcResult<Option<ReplicationOutcome<S>>>)>,
> {
    let rpc_timeout = config.raft.rpc_timeout();
    let consensus_timeout = config.raft.consensus_timeout();

    peer_manager
        .peer_ids()
        .into_iter()
        .map(|peer_id| {
            prepare_and_replicate_to_peer(
                state.clone(),
                peer_manager.clone(),
                peer_id,
                params,
                rpc_timeout,
                consensus_timeout,
            )
        })
        .collect()
}

/// Prepares and executes a single replication attempt for a specific peer.
///
/// This delegate function handles the "Tri-Layer" boundary:
/// 1. Logical Layer: Acquires the node state to build a customized payload for
///    the peer.
/// 2. Physical Layer: Transitions to the asynchronous network phase to transmit
///    the RPC.
/// 3. Safety: Enforces the Halt Mandate if arithmetic invariants are violated
///    during preparation.
async fn prepare_and_replicate_to_peer<S: StateMachine>(
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    params: ReplicationRoundParams,
    rpc_timeout: Duration,
    consensus_timeout: Duration,
) -> (NodeId, RpcResult<Option<ReplicationOutcome<S>>>) {
    // 1. Decision Phase: Determine which strategy to use while holding the lock.
    let strategy = {
        let mut guard = state.write().await;
        match determine_replication_strategy(&mut guard, peer_id, params) {
            Ok(Some(s)) => s,
            Ok(None) => return (peer_id, Ok(None)),
            Err(e) => {
                // If arithmetic fails here, it's a protocol violation.
                // We must poison and halt according to Rule 4.1 (ADR 009).
                let last_idx = guard.last_log_index();
                guard.apply_fatal(NodeError::Protocol(format!(
                    "Replication strategy failed for peer={} at index={} in term={}: {}",
                    peer_id, last_idx, params.term, e
                )));
            }
        }
    };

    // 2. Execution Phase: Execute the chosen strategy outside the consensus lock.
    // ADR 011: Heavy serialization or network I/O MUST NOT hold the consensus lock.
    let res = match strategy {
        ReplicationStrategy::AppendEntries(req) => {
            replicate_to_peer(peer_manager, peer_id, req, rpc_timeout, params.trace_id).await
        }
        ReplicationStrategy::InstallSnapshot {
            last_included_index,
            last_included_term,
        } => {
            // If a snapshot is already in flight for this peer, we downgrade
            // to a lightweight heartbeat to avoid redundant heavy work.
            if let Some(permit) = state.try_acquire_snapshot_permit(peer_id).await {
                replicate_snapshot_to_peer(
                    state,
                    peer_manager,
                    peer_id,
                    params,
                    last_included_index,
                    last_included_term,
                    rpc_timeout,
                    consensus_timeout,
                    permit,
                )
                .await
            } else {
                debug!(
                    target: ClinicalTarget::RaftReplication.as_str(),
                    peer = %peer_id,
                    "Snapshot already in flight. Downgrading to heartbeat probe."
                );
                let heartbeat = {
                    let mut guard = state.write().await;
                    build_append_entries_request(
                        &mut guard,
                        peer_id,
                        params.term,
                        params.node_id,
                        params.last_committed,
                    )
                };
                match heartbeat {
                    Ok(req) => {
                        replicate_to_peer(peer_manager, peer_id, req, rpc_timeout, params.trace_id)
                            .await
                    }
                    Err(e) => {
                        warn!(
                            target: ClinicalTarget::RaftFoundation.as_str(),
                            peer = %peer_id,
                            error = %e,
                            "Heartbeat construction failed; downgrading to empty probe"
                        );
                        Ok(None)
                    }
                }
            }
        }
    };

    (peer_id, res)
}

/// Orchestrates the heavy serialization and transmission of a state snapshot
/// to a lagging peer.
#[allow(clippy::too_many_arguments)]
async fn replicate_snapshot_to_peer<S: StateMachine>(
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    params: ReplicationRoundParams,
    last_included_index: LogIndex,
    last_included_term: Term,
    rpc_timeout: Duration,
    consensus_timeout: Duration,
    permit: SnapshotPermit<S>,
) -> RpcResult<Option<ReplicationOutcome<S>>> {
    // Phase 1: Reachability Probe (Lightweight)
    // We send a quiet heartbeat anchored at the snapshot horizon to verify
    // the follower is alive before performing heavy FSM serialization.
    let probe_req = AppendEntriesRequest::new(
        params.term,
        params.node_id,
        last_included_index,
        last_included_term,
        vec![], // No entries
        params.last_committed,
    );

    let probe_res = replicate_to_peer::<S>(
        peer_manager.clone(),
        peer_id,
        probe_req,
        rpc_timeout,
        params.trace_id,
    )
    .await;

    match probe_res {
        Err(e) => {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                peer = %peer_id,
                error = %e,
                "Snapshot target unresponsive to probe. Aborting heavy serialization."
            );
            return Ok(None);
        }
        Ok(Some(outcome)) => {
            // CLINICAL SAFETY (Raft §5.1): If the probe discovered a higher term,
            // we MUST return the outcome immediately so the orchestrator can
            // demote the leader. Proceeding to Phase 2 (Heavy Payload) in a stale
            // term is a violation of the Stability Invariant.
            let resp_term = match &outcome {
                ReplicationOutcome::AppendEntries { response, .. } => Term::new(response.term),
                ReplicationOutcome::InstallSnapshot { response, .. } => Term::new(response.term),
            };

            if resp_term > params.term {
                info!(
                    target: ClinicalTarget::RaftReplication.as_str(),
                    peer = %peer_id,
                    new_term = %resp_term,
                    "Probe discovered higher term. Returning outcome for immediate demotion."
                );
                return Ok(Some(outcome));
            }
        }
        Ok(None) => {
            // Standard heartbeat logic might return Ok(None) if the strategy
            // changed under lock, but for a manual probe we expect an outcome.
        }
    }

    // Phase 2: Heavy Payload (Serialization)
    let fsm = {
        let mut guard = state.write().await;
        if let Err(e) = state.freeze() {
            guard.apply_fatal(NodeError::Protocol(e.0.to_string()));
        }
        guard.fsm()
    };

    // Serialize FSM I/O with the background applier (ADR 009).
    // The lock is acquired inside spawn_blocking via blocking_lock()
    // to avoid lifetime conflicts with the 'static closure bound.
    let state_clone = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _fsm_guard = state_clone.fsm_lock.blocking_lock();
        fsm.snapshot()
    })
    .await;

    {
        let mut guard = state.write().await;
        if let Err(e) = state.thaw() {
            guard.apply_fatal(NodeError::Protocol(e.0.to_string()));
        }
    }

    let data = match res {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            // Trigger Poison-then-Panic with comprehensive forensics (Rule [SAFE-04])
            let mut guard = state.write().await;
            guard.apply_fatal(NodeError::Protocol(format!(
                "Snapshot serialization failed for peer={} at index={} in term={}: {}",
                peer_id, last_included_index, params.term, e
            )));
        }
        Err(e) => {
            return Err(Status::internal(format!("Snapshot spawn failure: {}", e)));
        }
    };

    let request = InstallSnapshotRequest::new(
        params.term,
        params.node_id,
        last_included_index,
        last_included_term,
        data,
    );

    install_snapshot_to_peer(
        peer_manager,
        peer_id,
        request,
        consensus_timeout,
        params.trace_id,
        permit,
    )
    .await
}

/// Executes a single AppendEntries RPC with strict causal verification.
///
/// Injects the current telemetry trace into the gRPC metadata and validates
/// the returned trace ID back from the peer, guarding against Byzantine
/// correlation failures (ADR 010). Returns a 'ReplicationOutcome' DTO for
/// leader reconciliation.
async fn replicate_to_peer<S: StateMachine>(
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    request: AppendEntriesRequest,
    timeout: Duration,
    trace_id: TraceId,
) -> RpcResult<Option<ReplicationOutcome<S>>> {
    let sent_prev_index = LogIndex::new(request.prev_log_index);
    let sent_entries_len = request.entries.len() as u64;

    let mut client = peer_manager.get_client(peer_id)?;

    let mut req = Request::new(request);
    // ADR 003: We set the timeout on the request itself...
    req.set_timeout(timeout);

    // Explicit Outbound Propagation (ADR 010)
    TraceInterceptor::inject_trace_id_into_request(&mut req, trace_id)
        .map_err(|e| Status::internal(format!("Telemetry injection failed: {}", e)))?;

    // ...but for reachability probes or dead-end connections, we ALSO
    // wrap the future in a tokio timeout to ensure we don't hang on
    // connection establishment.
    //
    // CONNECTION CUSHION: We use a multiplier for the global liveness
    // bound to allow the multi-stage gRPC handshake (TCP, TLS, HTTP/2) to
    // complete on cold starts without starving the RPC's actual processing
    // budget (Rule [ENG-03]).
    let liveness_timeout = timeout * CONNECTION_CUSHION_MULTIPLIER;
    let response_fut = tokio::time::timeout(liveness_timeout, client.append_entries(req));

    let response = response_fut
        .await
        .map_err(|_| Status::deadline_exceeded("RPC connection timeout"))??;

    // Causal Integrity Verification (ADR 010)
    verify_trace_integrity(&response, trace_id, peer_id)?;

    Ok(Some(ReplicationOutcome::AppendEntries {
        sent_prev_index,
        sent_entries_len,
        response: response.into_inner(),
    }))
}

/// Executes a single InstallSnapshot RPC with strict causal verification.
///
/// Injects the current telemetry trace into the gRPC metadata and validates
/// the returned trace ID back from the peer (ADR 010). Returns a
/// 'ReplicationOutcome' DTO for leader reconciliation.
async fn install_snapshot_to_peer<S: StateMachine>(
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    request: InstallSnapshotRequest,
    timeout: Duration,
    trace_id: TraceId,
    permit: SnapshotPermit<S>,
) -> RpcResult<Option<ReplicationOutcome<S>>> {
    let last_included_index = LogIndex::new(request.last_included_index);

    let mut client = peer_manager.get_client(peer_id)?;

    let mut req = Request::new(request);
    req.set_timeout(timeout);

    // Explicit Outbound Propagation (ADR 010)
    TraceInterceptor::inject_trace_id_into_request(&mut req, trace_id)
        .map_err(|e| Status::internal(format!("Telemetry injection failed: {}", e)))?;

    // Apply global timeout wrapper (Rule 15)
    let response_fut = tokio::time::timeout(timeout, client.install_snapshot(req));

    let response = response_fut
        .await
        .map_err(|_| Status::deadline_exceeded("RPC connection timeout"))??;

    // Causal Integrity Verification (ADR 010)
    verify_trace_integrity(&response, trace_id, peer_id)?;

    Ok(Some(ReplicationOutcome::InstallSnapshot {
        last_included_index,
        response: response.into_inner(),
        _permit: permit,
    }))
}

// =============================================================================
// VI. Specialized Sub-functions (Logic Delegates)
// =============================================================================

// --- Telemetry & Identity ---

/// Maps the physical node state to a semantic role name for telemetry spans.
fn determine_node_role_name<S: StateMachine>(node: &LogicalNode<S>) -> &'static str {
    match node.state() {
        RoleState::Follower(_) => "follower_session",
        RoleState::Candidate(_) => "candidate_session",
        RoleState::Leader(_) => "leader_idle_session",
        RoleState::Poisoned => "poisoned",
    }
}

// --- Replication & State Machine ---

/// Dynamically determines the appropriate replication strategy for a specific
/// peer.
///
/// REPLICATION STRATEGY (§5.3, §7):
/// 1. Log-Based: If the follower's `next_index` is within the leader's physical
///    log, send an `AppendEntries` RPC.
/// 2. Snapshot-Based: If the follower has fallen behind the leader's truncation
///    horizon, return the snapshot coordinates.
fn determine_replication_strategy<S: StateMachine>(
    node: &mut LogicalNode<S>,
    peer_id: NodeId,
    params: ReplicationRoundParams,
) -> Result<Option<ReplicationStrategy>, NodeError> {
    match node.state() {
        RoleState::Leader(n) => {
            let next_idx = *n
                .state()
                .next_index()
                .get(&peer_id)
                .unwrap_or(&LogIndex::new(1));
            let last_included = node.last_included_index();

            if next_idx <= last_included {
                // FALLBACK (Raft §7): next_index is behind leader's horizon.
                // We MUST send a snapshot instead.
                Ok(Some(ReplicationStrategy::InstallSnapshot {
                    last_included_index: last_included,
                    last_included_term: node.last_included_term(),
                }))
            } else {
                let req = build_append_entries_request(
                    node,
                    peer_id,
                    params.term,
                    params.node_id,
                    params.last_committed,
                )?;
                Ok(Some(ReplicationStrategy::AppendEntries(req)))
            }
        }
        _ => Ok(None),
    }
}

/// Dynamically constructs an AppendEntries payload for a specific peer.
///
/// Calculates the correct `prev_log_index` and `prev_log_term` based on the
/// peer's `next_index` state.
fn build_append_entries_request<S: StateMachine>(
    node: &mut LogicalNode<S>,
    peer_id: NodeId,
    term: Term,
    node_id: NodeId,
    last_committed: LogIndex,
) -> Result<AppendEntriesRequest, NodeError> {
    let next_idx = if let RoleState::Leader(n) = node.state() {
        *n.state()
            .next_index()
            .get(&peer_id)
            .unwrap_or(&LogIndex::new(1))
    } else {
        LogIndex::new(1)
    };
    let last_log_idx = node.last_log_index();

    let prev_log_index = (next_idx - 1)?;
    let prev_log_term = node.get_term_at(prev_log_index);

    let entries = node.read_entries(next_idx, last_log_idx);

    Ok(AppendEntriesRequest::new(
        term,
        node_id,
        prev_log_index,
        prev_log_term,
        entries,
        last_committed,
    ))
}

/// Computes the consensus quorum and advances the Leader's commit index.
///
/// Implements the commit-at-majority logic from §5.3, ensuring that the
/// commit index only advances for the current term to maintain safety.
fn update_leader_last_committed<S: StateMachine>(node: &mut LogicalNode<S>) {
    let last_idx = node.last_log_index();
    let current_term = node.current_term();
    let (median_idx, commit_idx) = if let RoleState::Leader(n) = node.state() {
        let mut match_indices: Vec<LogIndex> = n.state().match_index().values().cloned().collect();
        match_indices.push(last_idx); // Include self
        match_indices.sort_unstable();

        // The index that is replicated on a majority of nodes.
        let median = match_indices[(match_indices.len() - 1) / 2];
        (median, node.last_committed())
    } else {
        return;
    };

    if median_idx > commit_idx && node.get_term_at(median_idx) == current_term {
        info!(
            target: ClinicalTarget::RaftReplication.as_str(),
            index = %median_idx,
            term = %current_term,
            "Quorum reached. Advancing Leader commit index."
        );
        node.advance_last_committed(median_idx);
    }
}

// --- Security & Integrity ---

/// Validates causal integrity of incoming RPC responses (ADR 010).
///
/// Ensures the peer correctly extracted and returned the TraceId injected
/// during the request phase. Fails hard on mismatch to detect Byzantine
/// grafting.
fn verify_trace_integrity<T>(
    response: &tonic::Response<T>,
    expected_id: TraceId,
    peer_id: NodeId,
) -> RpcResult<()> {
    match TraceInterceptor::extract_trace_id_from_response(response) {
        Some(returned_id) if returned_id == expected_id => Ok(()),
        Some(returned_id) => {
            warn!(
                target: ClinicalTarget::ClinicalTelemetry.as_str(),
                expected = %expected_id,
                got = %returned_id,
                peer = %peer_id,
                "Causal Integrity Violation: Peer returned mismatched TraceId"
            );
            Err(Status::data_loss("Causal Integrity Violation"))
        }
        None => {
            warn!(
                target: ClinicalTarget::ClinicalTelemetry.as_str(),
                peer = %peer_id,
                "Causal Integrity Violation: Peer returned response without TraceId"
            );
            Err(Status::data_loss("Trace ID Missing in Response"))
        }
    }
}

// =============================================================================
// VII. Testing Suite (BDD Specification)
// =============================================================================

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use common::proto::v1::raft::LogEntry;
    use common::proto::v1::raft::consensus_service_server::ConsensusService;
    use common::proto::v1::raft::consensus_service_server::ConsensusServiceServer;
    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use common_rpc::HEADER_TRACE_ID;
    use futures::FutureExt;
    use futures::stream;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use tokio::sync::oneshot;
    use tonic::Response;
    use tonic::async_trait;
    use tonic::transport::Server;

    use super::*;
    use crate::storage::LogStorage;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;
    use crate::tick::TickThresholds;

    struct TestContext<S: StateMachine> {
        config: Arc<Config>,
        state: Arc<ConsensusShell<S>>,
        peer_manager: Arc<PeerManager>,
        mock_peer: Option<MockPeerHandle>,
    }

    struct MockPeerHandle {
        pub shutdown_tx: Option<oneshot::Sender<()>>,
        pub service: Arc<MockConsensusService>,
    }

    impl Drop for MockPeerHandle {
        /// Clinical RAII Trigger: Sends a non-blocking shutdown signal to the
        /// mock server during panic or scope exit (ADR 009).
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
        }
    }

    impl<S: StateMachine> TestContext<S> {
        /// Initializes a new test context fixture.
        ///
        /// If `with_remote_peer` is true, spawns a background gRPC mock server
        /// to simulate a remote node in the cluster topology.
        async fn setup_with_fsm(fsm: Arc<S>, with_remote_peer: bool) -> Self {
            let config = mock_config(50, 100);
            let id = Arc::new(NodeIdentity::new(
                ClusterId::try_new("test-cluster").unwrap(),
                NodeId::try_new(1).unwrap(),
            ));
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = StdRng::seed_from_u64(1);
            let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            let mut peer_manager =
                Arc::new(PeerManager::try_new(id.clone(), &HashMap::new()).unwrap());
            let mut mock_peer = None;

            if with_remote_peer {
                let service = Arc::new(MockConsensusService {
                    vote_response: Arc::new(Mutex::new(RequestVoteResponse::new(Term::ZERO, true))),
                    append_response: Arc::new(Mutex::new(AppendEntriesResponse::new(
                        Term::ZERO,
                        true,
                        LogIndex::ZERO,
                    ))),
                    snapshot_response: Arc::new(Mutex::new(InstallSnapshotResponse::new(
                        Term::ZERO,
                    ))),
                });

                let (tx, rx) = oneshot::channel::<()>();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let bound_addr = listener.local_addr().unwrap();

                let service_clone = service.clone();
                tokio::spawn(async move {
                    let incoming = stream::unfold(listener, |listener| async move {
                        let res = listener.accept().await.map(|(s, _)| s);
                        Some((res, listener))
                    });

                    Server::builder()
                        .add_service(ConsensusServiceServer::from_arc(service_clone))
                        .serve_with_incoming_shutdown(incoming, rx.map(|_| ()))
                        .await
                        .expect("Mock server failed");
                });

                let mut peer_map = HashMap::new();
                let peer_id = NodeId::try_new(2).unwrap();
                peer_map.insert(peer_id, format!("http://{}", bound_addr));
                peer_manager = Arc::new(PeerManager::try_new(id, &peer_map).unwrap());

                mock_peer = Some(MockPeerHandle {
                    shutdown_tx: Some(tx),
                    service,
                });
            }

            TestContext {
                config,
                state,
                peer_manager,
                mock_peer,
            }
        }
    }

    impl TestContext<MockFsm> {
        /// Specialized helper for standard consensus tests.
        async fn setup(with_remote_peer: bool) -> Self {
            Self::setup_with_fsm(Arc::new(MockFsm), with_remote_peer).await
        }
    }

    struct MockConsensusService {
        vote_response: Arc<Mutex<RequestVoteResponse>>,
        append_response: Arc<Mutex<AppendEntriesResponse>>,
        snapshot_response: Arc<Mutex<InstallSnapshotResponse>>,
    }

    #[async_trait]
    impl ConsensusService for MockConsensusService {
        async fn request_vote(
            &self,
            request: Request<RequestVoteRequest>,
        ) -> Result<Response<RequestVoteResponse>, Status> {
            let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
            let mut res = Response::new(*self.vote_response.lock().unwrap());
            if let Some(val) = trace_id_header {
                res.metadata_mut().insert(HEADER_TRACE_ID, val);
            }
            Ok(res)
        }

        async fn append_entries(
            &self,
            request: Request<AppendEntriesRequest>,
        ) -> Result<Response<AppendEntriesResponse>, Status> {
            let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
            let mut res = Response::new(*self.append_response.lock().unwrap());
            if let Some(val) = trace_id_header {
                res.metadata_mut().insert(HEADER_TRACE_ID, val);
            }
            Ok(res)
        }

        async fn install_snapshot(
            &self,
            request: Request<InstallSnapshotRequest>,
        ) -> Result<Response<InstallSnapshotResponse>, Status> {
            let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
            let mut res = Response::new(*self.snapshot_response.lock().unwrap());
            if let Some(val) = trace_id_header {
                res.metadata_mut().insert(HEADER_TRACE_ID, val);
            }
            Ok(res)
        }
    }

    #[derive(Debug, Default)]
    struct MockFsm;
    use common::types::errors::FsmError;
    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        fn install_snapshot(
            &self,
            _last_included_index: LogIndex,
            _data: &[u8],
            _trace_id: TraceId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn mock_config(min_ms: u64, max_ms: u64) -> Arc<Config> {
        let toml_str = format!(
            r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            peers = {{}}
            [raft]
            election_timeout_min_ms = {}
            election_timeout_max_ms = {}
            snapshot_threshold = 20
            [policy]
            veto_addr = "http://127.0.0.1:50060"
            veto_timeout_ms = 1000
        "#,
            min_ms, max_ms
        );
        Arc::new(toml::from_str(&toml_str).unwrap())
    }

    mod initiate_election {
        use super::*;

        mod campaign_lifecycle {
            use super::*;

            #[tokio::test]
            async fn should_transition_to_leader_when_quorum_reached() {
                let ctx = TestContext::setup(true).await;
                {
                    let node_id = ctx.state.read().await.identity().node_id();

                    // 1. Setup Candidate state
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                    }

                    let params = ElectionCampaignParams {
                        term: Term::new(1),
                        node_id,
                        last_log_index: LogIndex::ZERO,
                        last_log_term: Term::ZERO,
                        trace_id: TraceId::generate(),
                    };

                    // 4. Run election
                    initiate_election(
                        ctx.config.clone(),
                        ctx.state.clone(),
                        ctx.peer_manager.clone(),
                        params,
                    )
                    .await
                    .expect("Failed to run election in test");

                    // 5. Verify transition
                    {
                        let guard = ctx.state.read().await;
                        assert!(matches!(guard.state(), RoleState::Leader(_)));
                    }
                }
            }
        }

        mod when_storage_fails_during_term_check {
            use super::*;
            use crate::test_utils::FailingTermStorage;

            #[tokio::test]
            async fn returns_error_propagated_to_start_election_campaign() {
                let config = mock_config(50, 100);
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(FailingTermStorage::with_succeed_count(1));
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let node =
                    LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                        .unwrap();
                let state = Arc::new(ConsensusShell::new(node));
                let peer_manager = Arc::new(PeerManager::try_new(id, &HashMap::new()).unwrap());
                let params = ElectionCampaignParams {
                    term: Term::new(1),
                    node_id: NodeId::try_new(1).unwrap(),
                    last_log_index: LogIndex::ZERO,
                    last_log_term: Term::ZERO,
                    trace_id: TraceId::generate(),
                };

                let result = initiate_election(config, state, peer_manager, params).await;

                assert!(result.is_err(), "Expected Err from storage failure");
            }
        }
    }

    mod process_vote_response {
        use super::*;

        mod when_storage_fails_during_tally {
            use super::*;
            use crate::test_utils::FailingTermStorage;

            #[tokio::test]
            async fn triggers_halt_mandate() {
                let _config = mock_config(50, 100);
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                // Keep a handle to control when failure triggers
                let failing = Arc::new(FailingTermStorage::with_succeed_count(10));
                let storage: Arc<dyn LogStorage> = failing.clone();
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let node =
                    LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                        .unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Transition to candidate (so as_candidate_mut returns Some)
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                }

                // Arm the failure: the next current_term() call will fail
                failing.set_succeed_count(0);

                let peer_ids = vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()];
                let res = Ok(RequestVoteResponse::new(Term::new(1), true));

                let state_clone = state.clone();
                let handle = tokio::spawn(async move {
                    process_vote_response(
                        &state_clone,
                        Term::new(1),
                        &peer_ids,
                        NodeId::try_new(2).unwrap(),
                        res,
                    )
                    .await
                });

                let result = handle.await;
                assert!(result.is_err(), "Expected panic (Halt Mandate)");

                // Verify node is poisoned after the panic
                let guard = state.read().await;
                assert!(matches!(guard.state(), RoleState::Poisoned));
            }
        }

        mod discovering_higher_term {
            use super::*;
            #[tokio::test]
            async fn should_demote_to_follower_when_peer_has_newer_term() {
                let ctx = TestContext::setup(false).await;
                {
                    let res = Ok(RequestVoteResponse::new(Term::new(2), false));
                    let action = process_vote_response(
                        &ctx.state,
                        Term::new(1),
                        &ctx.peer_manager.peer_ids(),
                        NodeId::try_new(2).unwrap(),
                        res,
                    )
                    .await
                    .unwrap();
                    assert_eq!(action, VoteAction::Demoted);
                    assert_eq!(
                        ctx.state.read().await.try_current_term().unwrap(),
                        Term::new(2)
                    );
                }
            }
        }

        mod reaching_quorum {
            use super::*;
            #[tokio::test]
            async fn should_transition_to_leader_when_majority_votes_granted() {
                let ctx = TestContext::setup(false).await;
                {
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                    }
                    let res = Ok(RequestVoteResponse::new(Term::new(1), true));
                    let action = process_vote_response(
                        &ctx.state,
                        Term::new(1),
                        &[NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                        NodeId::try_new(2).unwrap(),
                        res,
                    )
                    .await
                    .unwrap();
                    assert_eq!(action, VoteAction::QuorumReached);
                    assert!(matches!(
                        ctx.state.read().await.state(),
                        RoleState::Leader(_)
                    ));
                }
            }
        }
    }

    mod replicate_to_peers {
        use super::*;

        mod broadcast_lifecycle {
            use super::*;

            #[tokio::test]
            async fn should_fan_out_to_all_peers_when_invoked() {
                let ctx = TestContext::setup(false).await;
                {
                    let p1 = NodeId::try_new(2).unwrap();
                    let p2 = NodeId::try_new(3).unwrap();

                    let mut peer_map = HashMap::new();
                    peer_map.insert(p1, "http://127.0.0.1:50091".to_string());
                    peer_map.insert(p2, "http://127.0.0.1:50092".to_string());

                    let pm = Arc::new(
                        PeerManager::try_new(ctx.state.read().await.identity(), &peer_map).unwrap(),
                    );

                    let params = ReplicationRoundParams {
                        term: Term::new(1),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::ZERO,
                        trace_id: TraceId::generate(),
                    };

                    let stream = broadcast_append_entries(
                        ctx.config.as_ref(),
                        pm,
                        ctx.state.clone(),
                        params,
                    );
                    assert_eq!(stream.len(), 2);
                }
            }
        }
    }

    mod process_replication_response {
        use super::*;

        mod when_storage_fails_during_term_check {
            use super::*;
            use crate::test_utils::FailingTermStorage;

            #[tokio::test]
            async fn triggers_halt_mandate() {
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let failing = Arc::new(FailingTermStorage::with_succeed_count(10));
                let storage: Arc<dyn LogStorage> = failing.clone();
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let node =
                    LogicalNode::try_new(id.clone(), Arc::new(MockFsm), storage, thresholds, rng)
                        .unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Transition to leader (so as_leader_mut returns Some)
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                }

                // Arm the failure: the next current_term() call will fail
                failing.set_succeed_count(0);

                let res = Ok(Some(ReplicationOutcome::AppendEntries {
                    sent_prev_index: LogIndex::new(0),
                    sent_entries_len: 1,
                    response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
                }));

                let state_clone = state.clone();
                let handle = tokio::spawn(async move {
                    process_replication_response(&state_clone, Term::new(1), peer_id, res).await
                });

                let result = handle.await;
                assert!(result.is_err(), "Expected panic (Halt Mandate)");

                let guard = state.read().await;
                assert!(matches!(guard.state(), RoleState::Poisoned));
            }
        }

        mod successful_replication {
            use super::*;
            #[tokio::test]
            async fn should_advance_indices_when_peer_accepts_entries() {
                let ctx = TestContext::setup(false).await;
                {
                    let peer_id = NodeId::try_new(2).unwrap();
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![peer_id]);
                    }
                    let res = Ok(Some(ReplicationOutcome::AppendEntries {
                        sent_prev_index: LogIndex::new(0),
                        sent_entries_len: 1,
                        response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
                    }));
                    process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                        .await
                        .expect("Failed to advance horizon in test");
                    let guard = ctx.state.read().await;
                    if let RoleState::Leader(node) = guard.state() {
                        assert_eq!(
                            *node.state().match_index().get(&peer_id).unwrap(),
                            LogIndex::new(1)
                        );
                        assert_eq!(
                            *node.state().next_index().get(&peer_id).unwrap(),
                            LogIndex::new(2)
                        );
                    } else {
                        panic!("Should be leader");
                    }
                }
            }
        }

        mod log_mismatch_handling {
            use super::*;
            #[tokio::test]
            async fn should_optimize_backoff_when_peer_rejects_due_to_mismatch() {
                let ctx = TestContext::setup(false).await;
                {
                    let peer_id = NodeId::try_new(2).unwrap();
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![peer_id]);
                        if let Some(node) = guard.as_leader_mut() {
                            node.state_mut()
                                .next_index_mut()
                                .insert(peer_id, LogIndex::new(11));
                        }
                    }
                    let res = Ok(Some(ReplicationOutcome::AppendEntries {
                        sent_prev_index: LogIndex::new(10),
                        sent_entries_len: 0,
                        response: AppendEntriesResponse::new(Term::new(1), false, LogIndex::new(5)),
                    }));
                    process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                        .await
                        .expect("Failed to advance horizon in test");
                    let guard = ctx.state.read().await;
                    if let RoleState::Leader(node) = guard.state() {
                        assert_eq!(
                            *node.state().next_index().get(&peer_id).unwrap(),
                            LogIndex::new(6)
                        );
                    } else {
                        panic!("Should be leader");
                    }
                }
            }
        }

        mod successful_snapshot_installation {
            use super::*;
            #[tokio::test]
            async fn should_advance_indices_to_snapshot_horizon_when_installation_succeeds() {
                let ctx = TestContext::setup(false).await;
                {
                    let peer_id = NodeId::try_new(2).unwrap();
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![peer_id]);
                    }

                    let snapshot_index = LogIndex::new(50);
                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let res = Ok(Some(ReplicationOutcome::InstallSnapshot {
                        last_included_index: snapshot_index,
                        response: InstallSnapshotResponse::new(Term::new(1)),
                        _permit: permit,
                    }));

                    process_replication_response(&ctx.state, Term::new(1), peer_id, res)
                        .await
                        .expect("Failed to advance horizon in test");

                    let guard = ctx.state.read().await;
                    if let RoleState::Leader(node) = guard.state() {
                        assert_eq!(
                            *node.state().match_index().get(&peer_id).unwrap(),
                            snapshot_index
                        );
                        assert_eq!(
                            *node.state().next_index().get(&peer_id).unwrap(),
                            LogIndex::new(51)
                        );
                    } else {
                        panic!("Should be leader");
                    }
                }
            }
        }
    }

    mod replicate_snapshot_to_peer {
        use super::*;

        mod higher_term_discovery {
            use super::*;
            #[tokio::test]
            async fn should_abort_and_return_outcome_when_probe_discovers_higher_term() {
                let ctx = TestContext::setup(true).await;
                {
                    let mock = ctx.mock_peer.as_ref().unwrap();

                    // Simulate higher term on peer
                    let higher_term = Term::new(10);
                    *mock.service.append_response.lock().unwrap() =
                        AppendEntriesResponse::new(higher_term, false, LogIndex::ZERO);

                    let last_included_index = LogIndex::new(10);
                    let last_included_term = Term::new(2);
                    let peer_id = NodeId::try_new(2).unwrap();
                    let params = ReplicationRoundParams {
                        term: Term::new(3), // Current term is 3, peer is 10
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    };

                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let res = replicate_snapshot_to_peer(
                        ctx.state.clone(),
                        ctx.peer_manager.clone(),
                        peer_id,
                        params,
                        last_included_index,
                        last_included_term,
                        Duration::from_secs(1),
                        Duration::from_secs(30),
                        permit,
                    )
                    .await;

                    // Verify: Returns Ok(Some(Outcome)) with the higher term
                    assert!(res.is_ok());
                    let outcome = res.unwrap().expect("Should return outcome");
                    if let ReplicationOutcome::AppendEntries { response, .. } = outcome {
                        assert_eq!(Term::new(response.term), higher_term);
                    } else {
                        panic!("Expected AppendEntries outcome from probe");
                    }

                    // Verify: FSM serialization was never triggered (flag is false)
                    assert!(!ctx.state.is_frozen());
                }
            }
        }

        mod normal_operation {
            use super::*;
            #[tokio::test]
            async fn should_proceed_when_probe_is_successful_with_current_term() {
                let ctx = TestContext::setup(true).await;
                {
                    let last_included_index = LogIndex::new(10);
                    let last_included_term = Term::new(2);
                    let peer_id = NodeId::try_new(2).unwrap();
                    let params = ReplicationRoundParams {
                        term: Term::new(3),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    };

                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let res = replicate_snapshot_to_peer(
                        ctx.state.clone(),
                        ctx.peer_manager.clone(),
                        peer_id,
                        params,
                        last_included_index,
                        last_included_term,
                        Duration::from_secs(1),
                        Duration::from_secs(30),
                        permit,
                    )
                    .await;

                    // Verify: Successfully completed Phase 1 and Phase 2
                    assert!(res.is_ok());
                    assert!(res.unwrap().is_some());
                }
            }

            #[tokio::test]
            async fn should_toggle_freeze_flag_during_serialization() {
                use std::sync::atomic::AtomicBool;
                use std::sync::atomic::Ordering;

                use tokio::sync::Mutex as TokioMutex;

                #[derive(Debug)]
                struct ObservantFsm {
                    shell: Arc<TokioMutex<Option<Arc<ConsensusShell<ObservantFsm>>>>>,
                    flag_during_snapshot: Arc<AtomicBool>,
                }

                impl StateMachine for ObservantFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::ZERO)
                    }

                    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                        Ok(())
                    }

                    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                        if let Some(shell) = self.shell.blocking_lock().as_ref() {
                            let flag = shell.is_frozen();
                            self.flag_during_snapshot.store(flag, Ordering::SeqCst);
                        }
                        Ok(vec![])
                    }

                    fn install_snapshot(
                        &self,
                        _idx: LogIndex,
                        _data: &[u8],
                        _tid: TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                let flag_during_snapshot = Arc::new(AtomicBool::new(false));
                let fsm = Arc::new(ObservantFsm {
                    shell: Arc::new(TokioMutex::new(None)),
                    flag_during_snapshot: flag_during_snapshot.clone(),
                });

                let ctx = TestContext::setup_with_fsm(fsm.clone(), true).await;
                fsm.shell.lock().await.replace(ctx.state.clone());

                {
                    let last_included_index = LogIndex::new(10);
                    let last_included_term = Term::new(2);
                    let peer_id = NodeId::try_new(2).unwrap();
                    let params = ReplicationRoundParams {
                        term: Term::new(3),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    };

                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let _ = replicate_snapshot_to_peer(
                        ctx.state.clone(),
                        ctx.peer_manager.clone(),
                        peer_id,
                        params,
                        last_included_index,
                        last_included_term,
                        Duration::from_secs(1),
                        Duration::from_secs(30),
                        permit,
                    )
                    .await;

                    assert!(
                        flag_during_snapshot.load(Ordering::SeqCst),
                        "Flag should be true during snapshot()"
                    );
                    assert!(
                        !ctx.state.is_frozen(),
                        "Flag should be false after snapshot completes"
                    );
                }
            }
        }
    }

    mod prepare_and_replicate_to_peer {
        use super::*;

        mod failure_handling {
            use super::*;
            #[tokio::test]
            #[should_panic(expected = "Snapshot serialization failed for peer=2 at index=10 in \
                                       term=3: Persistence failure: Simulated failure")]
            async fn should_apply_fatal_with_rich_forensic_context_when_snapshot_fails() {
                #[derive(Debug)]
                struct FailingFsm;
                impl StateMachine for FailingFsm {
                    type Error = FsmError;

                    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                        Ok(LogIndex::ZERO)
                    }

                    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                        Ok(())
                    }

                    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                        Err(FsmError::persistence("Simulated failure"))
                    }

                    fn install_snapshot(
                        &self,
                        _idx: LogIndex,
                        _data: &[u8],
                        _tid: TraceId,
                    ) -> Result<(), Self::Error> {
                        Ok(())
                    }
                }

                let fsm = Arc::new(FailingFsm);
                let ctx = TestContext::setup_with_fsm(fsm, true).await;

                {
                    let last_included_index = LogIndex::new(10);
                    let last_included_term = Term::new(2);
                    let peer_id = NodeId::try_new(2).unwrap();
                    let params = ReplicationRoundParams {
                        term: Term::new(3),
                        node_id: NodeId::try_new(1).unwrap(),
                        last_committed: LogIndex::new(0),
                        trace_id: TraceId::generate(),
                    };

                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let _ = replicate_snapshot_to_peer(
                        ctx.state.clone(),
                        ctx.peer_manager.clone(),
                        peer_id,
                        params,
                        last_included_index,
                        last_included_term,
                        Duration::from_secs(1),
                        Duration::from_secs(30),
                        permit,
                    )
                    .await;
                }
            }
        }
    }

    mod update_leader_last_committed {
        use super::*;

        mod quorum_commitment {
            use super::*;
            #[tokio::test]
            async fn should_advance_commit_index_when_majority_matches_index() {
                let ctx = TestContext::setup(false).await;
                {
                    let p2 = NodeId::try_new(2).unwrap();
                    let p3 = NodeId::try_new(3).unwrap();
                    {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![p2, p3]);
                        if let Some(leader) = guard.as_leader_mut() {
                            let entries: Vec<_> = (1..=5)
                                .map(|i| LogEntry {
                                    index: i,
                                    term: 1,
                                    data: vec![],
                                })
                                .collect();
                            leader.log_store().append_entries(entries).unwrap();
                        }
                    }
                    {
                        let mut guard = ctx.state.write().await;
                        if let Some(node) = guard.as_leader_mut() {
                            node.state_mut()
                                .match_index_mut()
                                .insert(p2, LogIndex::new(4));
                            node.state_mut()
                                .match_index_mut()
                                .insert(p3, LogIndex::new(1));
                        }
                        update_leader_last_committed(&mut guard);
                        assert_eq!(guard.last_committed(), LogIndex::new(4));
                    }
                }
            }
        }
    }

    mod verify_trace_integrity {
        use common_rpc::TraceInterceptor;
        use tonic::Response;

        use super::*;

        mod matching_traces {
            use super::*;
            #[test]
            fn should_pass_when_returned_trace_matches_expected() {
                let trace_id = TraceId::generate();
                let mut response = Response::new(());
                TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)
                    .expect("Should inject trace ID");
                assert!(
                    verify_trace_integrity(&response, trace_id, NodeId::try_new(2).unwrap())
                        .is_ok()
                );
            }
        }

        mod mismatched_traces {
            use super::*;
            #[test]
            fn should_fail_with_data_loss_when_returned_trace_differs() {
                let expected = TraceId::generate();
                let got = TraceId::generate();
                let mut response = Response::new(());
                TraceInterceptor::inject_trace_id_into_response(&mut response, got)
                    .expect("Should inject trace ID");
                let res = verify_trace_integrity(&response, expected, NodeId::try_new(2).unwrap());
                assert!(res.is_err());
                assert_eq!(res.unwrap_err().code(), tonic::Code::DataLoss);
            }
        }

        mod missing_traces {
            use super::*;
            #[test]
            fn should_fail_with_data_loss_when_trace_id_is_absent() {
                let expected = TraceId::generate();
                let response = Response::new(());
                let res = verify_trace_integrity(&response, expected, NodeId::try_new(2).unwrap());
                assert!(res.is_err());
                assert_eq!(res.unwrap_err().code(), tonic::Code::DataLoss);
            }
        }
    }

    mod leader_replication_fallback {
        use super::*;

        mod lagging_follower {
            use super::*;

            #[tokio::test]
            async fn should_fallback_to_install_snapshot_when_next_index_is_behind_horizon() {
                let ctx = TestContext::setup(false).await;
                {
                    let peer_id = NodeId::try_new(2).unwrap();

                    // 1. Setup Leader with a compacted log
                    let params = {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![peer_id]);

                        // Compact log up to index 10
                        guard.save_snapshot_metadata(LogIndex::new(10), Term::new(1));

                        // Set follower's next_index to 5 (behind the 10 horizon)
                        if let Some(leader) = guard.as_leader_mut() {
                            leader
                                .state_mut()
                                .next_index_mut()
                                .insert(peer_id, LogIndex::new(5));
                        }

                        ReplicationRoundParams {
                            term: Term::new(1),
                            node_id: NodeId::try_new(1).unwrap(),
                            last_committed: LogIndex::new(0),
                            trace_id: TraceId::generate(),
                        }
                    };

                    // 2. Verify intent identification logic (Locked phase)
                    let mut guard = ctx.state.write().await;
                    if let RoleState::Leader(_) = guard.state() {
                        let strategy = determine_replication_strategy(&mut guard, peer_id, params)
                            .unwrap()
                            .expect("Failed to advance horizon in test");

                        assert!(matches!(
                            strategy,
                            ReplicationStrategy::InstallSnapshot { .. }
                        ));

                        if let ReplicationStrategy::InstallSnapshot {
                            last_included_index,
                            ..
                        } = strategy
                        {
                            assert_eq!(last_included_index, LogIndex::new(10));
                        }
                    } else {
                        panic!("Should be leader");
                    }
                }
            }
        }
    }

    mod should_compact_log {
        use super::*;

        fn append_dummy_entries<S: StateMachine>(guard: &mut LogicalNode<S>, count: u64) {
            let entries: Vec<_> = (1..=count)
                .map(|i| LogEntry {
                    index: i,
                    term: 1,
                    data: vec![],
                })
                .collect();
            guard.log_store().append_entries(entries).unwrap();
        }

        mod triggers {
            use super::*;
            #[tokio::test]
            async fn should_trigger_compaction_when_applied_entries_exceed_threshold() {
                let ctx = TestContext::setup(false).await;
                {
                    let mut guard = ctx.state.write().await;

                    // Threshold is 20 in mock_config.
                    append_dummy_entries(&mut guard, 21);

                    // Advance applied index forward to trigger compaction
                    guard
                        .advance_horizon_after_snapshot(LogIndex::new(21))
                        .expect("Failed to advance horizon in test");

                    assert!(should_compact_log(&mut guard, &ctx.config, false));
                }
            }

            #[tokio::test]
            async fn should_not_trigger_compaction_when_applied_index_is_below_threshold() {
                let ctx = TestContext::setup(false).await;
                {
                    let mut guard = ctx.state.write().await;

                    append_dummy_entries(&mut guard, 5);

                    // applied = 5, last_included = 0. log_length = 5 <= 20.
                    guard
                        .advance_horizon_after_snapshot(LogIndex::new(5))
                        .expect("Failed to advance horizon in test");

                    assert!(!should_compact_log(&mut guard, &ctx.config, false));
                }
            }

            #[tokio::test]
            async fn should_not_trigger_compaction_when_log_is_long_but_applied_is_low() {
                let ctx = TestContext::setup(false).await;
                {
                    let mut guard = ctx.state.write().await;

                    // log_index = 100, applied = 5. threshold = 20.
                    // Under previous logic (log_index based), this would trigger.
                    // Under new logic (applied based), it should NOT trigger.
                    append_dummy_entries(&mut guard, 100);
                    guard
                        .advance_horizon_after_snapshot(LogIndex::new(5))
                        .expect("Failed to advance horizon in test");

                    assert!(!should_compact_log(&mut guard, &ctx.config, false));
                }
            }

            #[tokio::test]
            async fn should_inhibit_compaction_when_snapshot_is_in_progress() {
                let ctx = TestContext::setup(false).await;
                {
                    let mut guard = ctx.state.write().await;

                    append_dummy_entries(&mut guard, 25);

                    guard
                        .advance_horizon_after_snapshot(LogIndex::new(25))
                        .expect("Failed to advance horizon in test");
                    ctx.state.freeze().unwrap();

                    assert!(!should_compact_log(&mut guard, &ctx.config, true));
                }
            }
        }
    }

    mod reachability_first_snapshotting {
        use super::*;

        mod probe_lifecycle {
            use super::*;

            #[tokio::test]
            async fn should_abort_snapshot_when_reachability_probe_fails() {
                let ctx = TestContext::setup(false).await;
                {
                    let peer_id = NodeId::try_new(2).unwrap();

                    // 1. Setup Leader with lagging peer
                    let params = {
                        let mut guard = ctx.state.write().await;
                        guard.into_candidate();
                        guard.into_leader(vec![peer_id]);
                        guard.save_snapshot_metadata(LogIndex::new(10), Term::new(1));
                        if let Some(leader) = guard.as_leader_mut() {
                            leader
                                .state_mut()
                                .next_index_mut()
                                .insert(peer_id, LogIndex::new(5));
                        }
                        ReplicationRoundParams {
                            term: Term::new(1),
                            node_id: NodeId::try_new(1).unwrap(),
                            last_committed: LogIndex::new(0),
                            trace_id: TraceId::generate(),
                        }
                    };

                    // 2. Configure PeerManager with a dead-end address (will trigger hang/error)
                    let mut peer_map = HashMap::new();
                    peer_map.insert(peer_id, "http://127.0.0.1:1".to_string()); // Invalid port
                    let pm = Arc::new(
                        PeerManager::try_new(ctx.state.read().await.identity(), &peer_map).unwrap(),
                    );

                    // 3. Execute snapshot task
                    let permit = ctx
                        .state
                        .try_acquire_snapshot_permit(peer_id)
                        .await
                        .unwrap();
                    let res = replicate_snapshot_to_peer(
                        ctx.state.clone(),
                        pm,
                        peer_id,
                        params,
                        LogIndex::new(10),
                        Term::new(1),
                        Duration::from_millis(10), // Short timeout
                        Duration::from_secs(30),   // Long snapshot timeout
                        permit,
                    )
                    .await;

                    // 4. Verify: No error returned, but no snapshot outcome either
                    assert!(res.is_ok());
                    assert!(
                        res.unwrap().is_none(),
                        "Should have aborted before heavy work"
                    );

                    // 5. Verify: FSM was never frozen
                    assert!(!ctx.state.is_frozen());
                }
            }
        }

        mod spawn_background_applier {
            use super::*;

            #[derive(Debug)]
            struct PoisonApplyFsm;

            impl StateMachine for PoisonApplyFsm {
                type Error = FsmError;

                fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
                    Ok(LogIndex::ZERO)
                }

                fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
                    Err(FsmError::invariant("Simulated FSM apply failure"))
                }

                fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
                    Ok(vec![])
                }

                fn install_snapshot(
                    &self,
                    _index: LogIndex,
                    _data: &[u8],
                    _trace_id: common::types::trace::TraceId,
                ) -> Result<(), Self::Error> {
                    Ok(())
                }
            }

            #[tokio::test]
            async fn should_poison_node_when_fsm_apply_fails_via_background_applier() {
                let fsm = Arc::new(PoisonApplyFsm);
                let id = Arc::new(NodeIdentity::new(
                    ClusterId::try_new("test-cluster").unwrap(),
                    NodeId::try_new(1).unwrap(),
                ));
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = StdRng::seed_from_u64(1);
                let node = LogicalNode::try_new(id.clone(), fsm, storage.clone(), thresholds, rng)
                    .unwrap();
                let state = Arc::new(ConsensusShell::new(node));

                // Append and commit an entry so apply_committed has work to do.
                storage
                    .append_entries(vec![common::proto::v1::raft::LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![1],
                    }])
                    .unwrap();

                {
                    let mut guard = state.write().await;
                    guard.advance_last_committed(LogIndex::new(1));
                }

                // Spawn apply_committed in a separate task to catch the panic
                // from apply_fatal.
                let state_clone = state.clone();
                let handle = tokio::spawn(async move {
                    crate::orchestration::apply_committed(&state_clone).await;
                });

                let result = handle.await;
                assert!(
                    result.is_err(),
                    "Expected apply_committed to panic on FSM failure"
                );

                // Node should be poisoned after the fatal error.
                let guard = state.read().await;
                assert!(guard.is_poisoned());
            }
        }
    }
}
