use std::sync::Arc;

use common::proto::v1::raft::PreVoteRequest;
use common::proto::v1::raft::PreVoteResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use common_rpc::TraceInterceptor;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tonic::Request;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;

use super::types::ConsensusResult;
use super::types::ElectionCampaignParams;
use super::types::PreVoteCampaignParams;
use super::types::RpcResult;
use super::types::VoteAction;
use super::types::VoteRequestParams;
use crate::config::Config;
use crate::engine::RoleState;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;

/// Spawns an asynchronous task to orchestrate an election campaign.
///
/// Establishes the 'election_campaign' telemetry context parented to the
/// current role session, ensuring causal linkage (ADR 010).
pub(super) fn start_election_campaign<S: StateMachine>(
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
pub(super) async fn initiate_election<S: StateMachine>(
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
pub(super) async fn process_vote_response<S: StateMachine>(
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
    super::rpc::verify_trace_integrity(&response, params.trace_id, peer_id)?;

    Ok(response.into_inner())
}

// =============================================================================
// Phase 8: Pre-Vote Campaign (Election Safety)
// =============================================================================

/// Spawns an asynchronous task to orchestrate a pre-vote campaign.
///
/// Parented to the current role session telemetry context (ADR 010).
pub(crate) fn start_pre_vote_campaign<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: PreVoteCampaignParams,
    parent_span: tracing::Span,
) {
    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        parent: &parent_span,
        "pre_vote_campaign",
        trace_id = %params.trace_id,
        term = %params.term
    );

    tokio::spawn(
        async move {
            if let Err(e) =
                initiate_pre_vote(config, state.clone(), peer_manager, params).await
            {
                error!( error = %e, "Failed to execute pre-vote campaign");
                let mut guard = state.write().await;
                guard.apply_fatal(e);
            }
        }
        .instrument(span),
    );
}

/// Orchestrates a Pre-Vote Campaign by soliciting dry-run votes from peers.
///
/// Unlike a real election, pre-vote does NOT advance the term, does NOT count
/// a self-vote, and does NOT demote on higher term responses. If quorum is
/// reached, the node transitions to Candidate (which advances the term) and
/// the tick loop triggers StartElection. Otherwise the campaign times out and
/// the PreCandidate's evaluate_tick returns StepDown.
#[instrument(
    name = "pre_vote_campaign_execution",
    target = "raft::foundation",
    skip_all,
    fields(term = %params.term, trace_id = %params.trace_id)
)]
pub(super) async fn initiate_pre_vote<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: PreVoteCampaignParams,
) -> ConsensusResult<()> {
    info!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        last_log_index = %params.last_log_index,
        last_log_term = %params.last_log_term,
        term = %params.term,
        "Starting pre-vote campaign."
    );

    let peer_ids = peer_manager.peer_ids();
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;

    let mut pre_vote_stream = broadcast_pre_vote_requests(
        config.as_ref(),
        peer_manager.clone(),
        params.term,
        params.node_id,
        params.last_log_index,
        params.last_log_term,
        params.trace_id,
    );

    let mut pre_votes_granted = 0; // No self-vote in pre-election

    while let Some((_peer_id, res)) = pre_vote_stream.next().await {
        let granted = process_pre_vote_response(res)?;
        if granted {
            pre_votes_granted += 1;
            if pre_votes_granted >= quorum {
                let mut guard = state.write().await;
                guard.into_candidate();
                info!(
                    target: ClinicalTarget::RaftFoundation.as_str(),
                    votes = %pre_votes_granted,
                    quorum = %quorum,
                    "Pre-vote quorum reached. Transitioning to Candidate."
                );
                return Ok(());
            }
        }
    }

    info!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        votes = %pre_votes_granted,
        quorum = %quorum,
        "Pre-vote campaign finished without quorum."
    );

    Ok(())
}

/// Evaluates a single pre-vote response. Returns true if pre-vote granted.
///
/// Phase 8: No higher term demotion — pre-vote is read-only.
fn process_pre_vote_response(
    res: RpcResult<PreVoteResponse>,
) -> ConsensusResult<bool> {
    match res {
        Ok(resp) => {
            if resp.vote_granted {
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(e) => {
            debug!(
                target: ClinicalTarget::RaftFoundation.as_str(),
                error = %e,
                "Failed to get pre-vote from peer"
            );
            Ok(false)
        }
    }
}

/// Broadcasts PreVote RPCs concurrently to all cluster peers.
fn broadcast_pre_vote_requests(
    config: &Config,
    peer_manager: Arc<PeerManager>,
    term: Term,
    node_id: NodeId,
    last_log_index: LogIndex,
    last_log_term: Term,
    trace_id: TraceId,
) -> FuturesUnordered<impl futures::Future<Output = (NodeId, RpcResult<PreVoteResponse>)>> {
    let params = PreVoteCampaignParams {
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
            async move { (peer_id, request_pre_vote_from_peer(pm, peer_id, params).await) }
        })
        .collect()
}

/// Executes a single PreVote RPC with causal verification.
async fn request_pre_vote_from_peer(
    peer_manager: Arc<PeerManager>,
    peer_id: NodeId,
    params: PreVoteCampaignParams,
) -> RpcResult<PreVoteResponse> {
    let mut client = peer_manager.get_client(peer_id)?;

    let mut request = Request::new(PreVoteRequest::new(
        params.term,
        params.node_id,
        params.last_log_index,
        params.last_log_term,
    ));
    request.set_timeout(params.rpc_timeout);

    TraceInterceptor::inject_trace_id_into_request(&mut request, params.trace_id)
        .map_err(|e| Status::internal(format!("Telemetry injection failed: {}", e)))?;

    let response = client.pre_vote(request).await?;

    super::rpc::verify_trace_integrity(&response, params.trace_id, peer_id)?;

    Ok(response.into_inner())
}
