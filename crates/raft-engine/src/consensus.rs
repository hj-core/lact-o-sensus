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

use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::raft_api::StateMachine;
use common::rpc::TraceInterceptor;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
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

// =============================================================================
// I. Semantic Vocabulary (Types & Enums)
// =============================================================================

/// Standardized result type for internal Raft RPC operations.
///
/// Distinguishes transient network or protocol errors (Status) from
/// terminal system-level failures (NodeError).
type RpcResult<T> = std::result::Result<T, Status>;

/// Standardized result type for consensus orchestration logic.
type ConsensusResult<T> = std::result::Result<T, NodeError>;

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
struct ReplicationOutcome {
    sent_prev_index: LogIndex,
    sent_entries_len: u64,
    response: AppendEntriesResponse,
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
) {
    let interval = config.raft.tick_interval();
    let span = tracing::Span::current();

    tokio::spawn(
        async move {
            let mut current_session: Option<(tracing::Span, String, Term)> = None;

            loop {
                // 1. Fixed-interval pulse
                sleep(interval).await;

                // 2. Drive the logical engine and capture the required action
                // Perform state transitions (Atomic Handoff) while holding the lock.
                let (action, role_name, term, campaign, replication) = {
                    let mut guard = state.write().await;
                    let action = guard.tick();
                    let role = determine_node_role_name(&guard);
                    let term = guard.try_current_term().unwrap_or(Term::ZERO);

                    let mut campaign_params = None;
                    let mut replication_params = None;

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
    );
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

    let state_clone = state.clone();
    let peer_manager_clone = peer_manager.clone();
    let config_clone = config.clone();

    tokio::spawn(
        async move {
            if let Err(e) =
                initiate_election(config_clone, state_clone, peer_manager_clone, params).await
            {
                error!( error = %e, "Failed to execute election campaign");
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
        &config,
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
        let current = guard.try_current_term().unwrap_or(Term::ZERO);
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

        #[allow(clippy::collapsible_if)]
        if let Some(node) = guard.as_candidate_mut() {
            if node.current_term().unwrap_or(Term::ZERO) == term {
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
            if let Err(e) = replicate_to_peers(config, state, peer_manager, params).await {
                error!( error = %e, "Failed to replicate to peers");
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
        if process_append_entries_response(&state, params.term, peer_id, res).await?
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
async fn process_append_entries_response<S: StateMachine>(
    state: &ConsensusShell<S>,
    term: Term,
    peer_id: NodeId,
    res: RpcResult<Option<ReplicationOutcome>>,
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

    let sent_prev_index = outcome.sent_prev_index;
    let sent_entries_len = outcome.sent_entries_len;
    let resp = outcome.response;

    let mut guard = state.write().await;
    let resp_term = Term::new(resp.term);

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
    #[allow(clippy::collapsible_if)]
    if let Some(node) = guard.as_leader_mut() {
        if node.current_term().unwrap_or(Term::ZERO) == term {
            // Acknowledge read quorums (§8)
            let total_nodes = node.state().next_index().len() + 1;
            let quorum = (total_nodes / 2) + 1;
            node.state_mut().acknowledge_heartbeat(peer_id, quorum);

            if resp.success {
                let new_match = (sent_prev_index + sent_entries_len)?;
                let new_next = (new_match + 1)?;

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

                let last_log_index = LogIndex::new(resp.last_log_index);
                let new_next = if last_log_index > LogIndex::ZERO {
                    std::cmp::min(current_next, (last_log_index + 1)?)
                } else {
                    (current_next - 1).map(|idx| idx.max(LogIndex::new(1)))?
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
    }

    // 3. Opportunistically advance commit index if progress was made
    if last_committed_updated {
        update_leader_last_committed(&mut guard).await;
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
    let mut client = peer_manager
        .get_client(peer_id)
        .map_err(|e| Status::internal(e.to_string()))?;

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
) -> FuturesUnordered<impl futures::Future<Output = (NodeId, RpcResult<Option<ReplicationOutcome>>)>>
{
    let rpc_timeout = config.raft.rpc_timeout();

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
) -> (NodeId, RpcResult<Option<ReplicationOutcome>>) {
    // 1. Prepare the request for this specific peer (Synchronous locked phase)
    let request = {
        let mut guard = state.write().await;
        match guard.state() {
            RoleState::Leader(_) => {
                match build_append_entries_request(
                    &mut guard,
                    peer_id,
                    params.term,
                    params.node_id,
                    params.last_committed,
                ) {
                    Ok(req) => req,
                    Err(e) => {
                        // If arithmetic fails here, it's a protocol violation.
                        // We must poison and halt according to Rule 4.1.
                        guard.poison();
                        error!(
                            target: ClinicalTarget::ClinicalFoundation.as_str(),
                            error = %e,
                            peer = %peer_id,
                            "TERMINAL INVARIANT VIOLATION: Arithmetic failure during replication preparation. Halting."
                        );
                        panic!(
                            "Secure Clinical: Terminal Invariant Violation during replication: {}",
                            e
                        );
                    }
                }
            }
            _ => return (peer_id, Ok(None)),
        }
    };

    // 2. Execute RPC (Asynchronous network phase)
    (
        peer_id,
        replicate_to_peer(peer_manager, peer_id, request, rpc_timeout, params.trace_id).await,
    )
}

/// Executes a single AppendEntries RPC with strict causal verification.
///
/// Injects the current telemetry trace into the gRPC metadata and validates
/// the returned trace ID back from the peer, guarding against Byzantine
/// correlation failures (ADR 010). Returns a 'ReplicationOutcome' DTO for
/// leader reconciliation.
async fn replicate_to_peer(
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    request: AppendEntriesRequest,
    rpc_timeout: Duration,
    trace_id: TraceId,
) -> RpcResult<Option<ReplicationOutcome>> {
    let sent_prev_index = LogIndex::new(request.prev_log_index);
    let sent_entries_len = request.entries.len() as u64;

    let mut client = peer_manager
        .get_client(peer_id)
        .map_err(|e| Status::internal(e.to_string()))?;

    let mut req = Request::new(request);
    req.set_timeout(rpc_timeout);

    // Explicit Outbound Propagation
    TraceInterceptor::inject_trace_id_into_request(&mut req, trace_id)
        .map_err(|e| Status::internal(format!("Telemetry injection failed: {}", e)))?;

    let response = client.append_entries(req).await?;

    // Causal Integrity Verification (ADR 010)
    verify_trace_integrity(&response, trace_id, peer_id)?;

    Ok(Some(ReplicationOutcome {
        sent_prev_index,
        sent_entries_len,
        response: response.into_inner(),
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
) -> std::result::Result<AppendEntriesRequest, NodeError> {
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
async fn update_leader_last_committed<S: StateMachine>(node: &mut LogicalNode<S>) {
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
        node.advance_last_committed(median_idx).await;
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

    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use tonic::async_trait;

    use super::*;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;
    use crate::tick::TickThresholds;

    #[derive(Debug, Default)]
    struct MockFsm;
    use common::types::errors::FsmError;
    #[async_trait]
    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> std::result::Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(
            &self,
            _index: LogIndex,
            _data: &[u8],
        ) -> std::result::Result<(), Self::Error> {
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
            [policy]
            veto_addr = "http://127.0.0.1:50060"
            veto_timeout_ms = 1000
        "#,
            min_ms, max_ms
        );
        Arc::new(toml::from_str(&toml_str).unwrap())
    }

    async fn setup() -> (Arc<Config>, Arc<ConsensusShell<MockFsm>>, Arc<PeerManager>) {
        let config = mock_config(50, 100);
        let id = Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(1).unwrap(),
        ));
        let fsm = Arc::new(MockFsm);
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = StdRng::seed_from_u64(1);
        let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
        let state = Arc::new(ConsensusShell::new(node));
        let peer_manager = Arc::new(PeerManager::new(id, &HashMap::new()).unwrap());
        (config, state, peer_manager)
    }

    mod process_vote_response {
        use super::*;

        mod discovering_higher_term {
            use super::*;
            #[tokio::test]
            async fn should_demote_to_follower_when_peer_has_newer_term() {
                let (_, state, pm) = setup().await;
                let res = Ok(RequestVoteResponse::new(Term::new(2), false));
                let action = process_vote_response(
                    &state,
                    Term::new(1),
                    &pm.peer_ids(),
                    NodeId::try_new(2).unwrap(),
                    res,
                )
                .await
                .unwrap();
                assert_eq!(action, VoteAction::Demoted);
                assert_eq!(state.read().await.try_current_term().unwrap(), Term::new(2));
            }
        }

        mod reaching_quorum {
            use super::*;
            #[tokio::test]
            async fn should_transition_to_leader_when_majority_votes_granted() {
                let (_, state, _) = setup().await;
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                }
                let res = Ok(RequestVoteResponse::new(Term::new(1), true));
                let action = process_vote_response(
                    &state,
                    Term::new(1),
                    &[NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                    NodeId::try_new(2).unwrap(),
                    res,
                )
                .await
                .unwrap();
                assert_eq!(action, VoteAction::QuorumReached);
                assert!(matches!(state.read().await.state(), RoleState::Leader(_)));
            }
        }
    }

    mod process_append_entries_response {
        use super::*;

        mod successful_replication {
            use super::*;
            #[tokio::test]
            async fn should_advance_indices_when_peer_accepts_entries() {
                let (_, state, _) = setup().await;
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                }
                let res = Ok(Some(ReplicationOutcome {
                    sent_prev_index: LogIndex::new(0),
                    sent_entries_len: 1,
                    response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
                }));
                process_append_entries_response(&state, Term::new(1), peer_id, res)
                    .await
                    .unwrap();
                let guard = state.read().await;
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

        mod log_mismatch_handling {
            use super::*;
            #[tokio::test]
            async fn should_optimize_backoff_when_peer_rejects_due_to_mismatch() {
                let (_, state, _) = setup().await;
                let peer_id = NodeId::try_new(2).unwrap();
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![peer_id]);
                    if let Some(node) = guard.as_leader_mut() {
                        node.state_mut()
                            .next_index_mut()
                            .insert(peer_id, LogIndex::new(11));
                    }
                }
                let res = Ok(Some(ReplicationOutcome {
                    sent_prev_index: LogIndex::new(10),
                    sent_entries_len: 0,
                    response: AppendEntriesResponse::new(Term::new(1), false, LogIndex::new(5)),
                }));
                process_append_entries_response(&state, Term::new(1), peer_id, res)
                    .await
                    .unwrap();
                let guard = state.read().await;
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

    mod update_leader_last_committed {
        use super::*;

        mod quorum_commitment {
            use super::*;
            #[tokio::test]
            async fn should_advance_commit_index_when_majority_matches_index() {
                let (_, state, _) = setup().await;
                let p2 = NodeId::try_new(2).unwrap();
                let p3 = NodeId::try_new(3).unwrap();
                {
                    let mut guard = state.write().await;
                    guard.into_candidate();
                    guard.into_leader(vec![p2, p3]);
                    if let Some(leader) = guard.as_leader_mut() {
                        let entries: Vec<_> = (1..=5)
                            .map(|i| common::proto::v1::raft::LogEntry {
                                index: i,
                                term: 1,
                                data: vec![],
                            })
                            .collect();
                        leader.log_store().append_entries(entries).unwrap();
                    }
                }
                {
                    let mut guard = state.write().await;
                    if let Some(node) = guard.as_leader_mut() {
                        node.state_mut()
                            .match_index_mut()
                            .insert(p2, LogIndex::new(4));
                        node.state_mut()
                            .match_index_mut()
                            .insert(p3, LogIndex::new(1));
                    }
                    update_leader_last_committed(&mut guard).await;
                    assert_eq!(guard.last_committed(), LogIndex::new(4));
                }
            }
        }
    }

    mod verify_trace_integrity {
        use common::rpc::TraceInterceptor;
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
}
