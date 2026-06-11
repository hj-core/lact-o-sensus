//! Consensus Lifecycle Orchestration
//!
//! Houses the top-level background tasks that drive the Raft node's
//! deterministic lifecycle: the unified Tick Loop (ADR 003), the
//! background State Machine applier, and the asynchronous log compaction
//! orchestrator (ADR 011).

use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tokio::time::sleep;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;

use super::election::start_election_campaign;
use super::election::start_pre_vote_campaign;
use super::replication::initiate_replication;
use super::rpc::determine_node_role_name;
use super::types::ElectionCampaignParams;
use super::types::PreVoteCampaignParams;
use super::types::ReplicationRoundParams;
use crate::config::Config;
use crate::engine::LogicalNode;
use crate::engine::TickAction;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;

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
                let (
                    action,
                    role_name,
                    term,
                    cluster_id,
                    node_id,
                    campaign,
                    pre_vote_campaign,
                    replication,
                ) = {
                    let mut guard = state.write().await;
                    let action = guard.tick();
                    let role = determine_node_role_name(&guard);
                    let term = guard.current_term();

                    let mut campaign_params = None;
                    let mut pre_vote_params = None;
                    let mut replication_params = None;

                    // COMPACTION TRIGGER (ADR 011)
                    if should_compact_log(&mut guard, &config, state.is_frozen()) {
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
                        TickAction::StartPreVote => {
                            let trace_id = TraceId::generate();
                            guard.into_pre_candidate();
                            pre_vote_params = Some(PreVoteCampaignParams {
                                term,
                                node_id: guard.node_id(),
                                last_log_index: guard.last_log_index(),
                                last_log_term: guard.last_log_term(),
                                rpc_timeout: config.raft.rpc_timeout(),
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
                        guard.cluster_id().clone(),
                        guard.node_id(),
                        campaign_params,
                        pre_vote_params,
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
                        term = %term,
                        cluster_id = %cluster_id,
                        node_id = %node_id,
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
                    TickAction::StartPreVote => {
                        if let Some(params) = pre_vote_campaign {
                            start_pre_vote_campaign(
                                config.clone(),
                                state.clone(),
                                peer_manager.clone(),
                                params,
                                session_span,
                            );
                        }
                    }
                    TickAction::StepDown => {
                        let mut guard = state.write().await;
                        let current_term = guard.current_term();
                        guard.into_follower(current_term, None);
                        info!(
                            target: ClinicalTarget::RaftFoundation.as_str(),
                            "Pre-vote campaign timeout. Returning to Follower."
                        );
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
    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        "background_applier"
    );
    tokio::spawn(
        async move {
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
        }
        .instrument(span),
    );
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
pub(super) fn should_compact_log<S: StateMachine>(
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

/// Triggers an asynchronous log compaction cycle.
///
/// COMPACTION MECHANICS (ADR 011):
/// In implementations with persistent State Machines, the durable database
/// trees serve as the inherent snapshot. This orchestrator updates the
/// logical horizon metadata and truncates physical log entries that have
/// already been applied to the underlying storage. Peer nodes requiring
/// catch-up will trigger on-demand serialization via StateMachine::snapshot().
pub(crate) fn initiate_log_compaction<S: StateMachine>(
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
