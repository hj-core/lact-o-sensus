//! Consensus Replication Orchestration
//!
//! Implements the Raft leader's log replication and snapshot transport
//! responsibilities (§5.3, §7). Coordinates the fan-out of AppendEntries
//! and InstallSnapshot RPCs to cluster peers, processes their responses
//! for quorum commitment, and manages the snapshot serialization lifecycle.

use std::cmp;
use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::InstallSnapshotRequest;
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
use tonic::Request;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;

use super::rpc::build_append_entries_request;
use super::rpc::determine_replication_strategy;
use super::rpc::update_leader_last_committed;
use super::rpc::verify_trace_integrity;
use super::types::CONNECTION_CUSHION_MULTIPLIER;
use super::types::ConsensusResult;
use super::types::ReplicationAction;
use super::types::ReplicationOutcome;
use super::types::ReplicationRoundParams;
use super::types::ReplicationStrategy;
use super::types::RpcResult;
use crate::config::Config;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;
use crate::shell::SnapshotPermit;

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
pub(super) async fn process_replication_response<S: StateMachine>(
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

/// Broadcasts AppendEntries RPCs concurrently to all cluster peers.
///
/// Acts as a high-level orchestrator for the replication fan-out, delegating
/// the per-peer request preparation and network handling to
/// `prepare_and_replicate_to_peer`.
pub(super) fn broadcast_append_entries<S: StateMachine>(
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
pub(super) async fn prepare_and_replicate_to_peer<S: StateMachine>(
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
            if let Some(permit) = state.try_acquire_snapshot_permit(peer_id) {
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
pub(super) async fn replicate_snapshot_to_peer<S: StateMachine>(
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
    let span = tracing::Span::current();
    let res = tokio::task::spawn_blocking(move || {
        let _enter = span.enter();
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
