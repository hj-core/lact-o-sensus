//! Node Logic Orchestration for the Raft Engine.
//!
//! This module contains the higher-level orchestration functions that drive
//! the consensus state machine through multi-phase operations. These were
//! extracted from `ConsensusShell` to keep the shell focused on pure
//! coordination (locks, signals, permits, freeze management).

use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::Term;
use common::types::errors::ConsensusError;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tonic::Status;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;

use crate::config::Config;
use crate::consensus::ReplicationRoundParams;
use crate::consensus::initiate_replication;
use crate::engine::NodeRole;
use crate::engine::SnapshotAction;
use crate::peer::PeerManager;
use crate::service::consensus::SnapshotParams;
use crate::shell::ConsensusShell;

/// Orchestrates the non-blocking application of committed entries to the
/// State Machine.
///
/// This function ensures that the primary consensus lock is NOT held across
/// the FSM application boundary, preventing heartbeat starvation (ADR 009).
#[instrument(
    name = "apply_committed_orchestration",
    target = "raft::replication",
    skip(shell)
)]
pub(crate) async fn apply_committed<S: StateMachine>(shell: &Arc<ConsensusShell<S>>) {
    // [SERIALIZATION]: Ensure only one application loop runs at a time.
    // This prevents the race condition where concurrent tasks try to apply
    // the same log entry twice.
    let _permit = shell.fsm_lock.lock().await;

    let (fsm, log_store, mut applied, mut committed) = {
        let guard = shell.read().await;
        (
            guard.fsm(),
            guard.log_store(),
            guard.last_applied(),
            guard.last_committed(),
        )
    };

    // Fatal Invariant Violation: Application must never exceed commitment.
    if applied > committed {
        let mut guard = shell.write().await;
        guard.apply_fatal(NodeError::Protocol(format!(
            "Causal Divergence: applied ({}) > committed ({})",
            applied, committed
        )));
    }

    // Sequential application loop
    while applied < committed {
        let next_idx = match applied + 1u64 {
            Ok(idx) => idx,
            Err(e) => {
                let mut guard = shell.write().await;
                guard.apply_fatal(NodeError::Arithmetic(e));
            }
        };

        // Phase 1: Read and Apply (Unlocked)
        // We read directly from the persistent log store. Since committed entries
        // are immutable, this is safe to do without the primary consensus lock.
        let entry = match log_store.read_entries(next_idx, next_idx) {
            Ok(entries) => entries.into_iter().next(),
            Err(e) => {
                let mut guard = shell.write().await;
                guard.apply_fatal(NodeError::from(e));
            }
        };

        let apply_res = if let Some(entry) = entry {
            let fsm = fsm.clone();
            let data = entry.data.clone();
            match tokio::task::spawn_blocking(move || fsm.apply(next_idx, &data)).await {
                Ok(result) => result,
                Err(join_err) => {
                    let mut guard = shell.write().await;
                    guard.apply_fatal(NodeError::Protocol(format!(
                        "spawn_blocking join error: {}",
                        join_err
                    )));
                }
            }
        } else {
            let mut guard = shell.write().await;
            guard.apply_fatal(NodeError::Protocol(format!(
                "Committed entry {} missing from log storage",
                next_idx
            )));
        };

        // Phase 2: Advance Horizon (Locked)
        {
            let mut guard = shell.write().await;
            match apply_res {
                Ok(_) => {
                    // Update volatile cache.
                    match guard.advance_horizon_after_snapshot(next_idx) {
                        Ok(_) => {
                            applied = next_idx;
                            committed = guard.last_committed();
                        }
                        Err(e) => {
                            error!(index = %next_idx, error = %e, "Failed to advance horizon");
                            guard.apply_fatal(e);
                        }
                    }
                }
                Err(e) => {
                    error!(index = %next_idx, error = %e, "FSM Apply failed");
                    guard.apply_fatal(NodeError::Protocol(format!("Apply failure: {}", e)));
                }
            }
        }
    }
}

/// Orchestrates an InstallSnapshot request using a Non-Blocking Handoff.
///
/// PHASE 1 (Locked): Validation and logical coordination.
/// PHASE 2 (Unlocked): Offloaded background state machine restoration.
/// PHASE 3 (Locked): Finalization and Freeze-Apply toggle.
#[instrument(
    name = "handle_install_snapshot",
    target = "raft::compaction",
    skip_all
)]
pub(crate) async fn handle_install_snapshot<S: StateMachine>(
    shell: &Arc<ConsensusShell<S>>,
    params: SnapshotParams,
) -> Result<Term, Status> {
    // Phase 1: Lock & Validate
    let (action, current_term, fsm) = {
        let mut guard = shell.write().await;
        let res = guard.handle_install_snapshot(
            params.leader_id,
            params.term,
            params.last_included_index,
        );

        match res.action {
            SnapshotAction::Rejected => return Ok(res.term),
            SnapshotAction::Stale => return Ok(res.term),
            SnapshotAction::Accepted => {}
        }

        // Snapshot Accepted: Set Freeze-Apply state
        if let Err(e) = shell.freeze() {
            guard.apply_fatal(NodeError::Protocol(format!(
                "Structural Invariant Violation: {}",
                e.0
            )));
        }
        (res.action, res.term, guard.fsm())
    };

    // Phase 2: Background restoration (Unlocked)
    if action == SnapshotAction::Accepted {
        let shell_clone = shell.clone();
        let index = params.last_included_index;
        let term = params.last_included_term;
        let data = params.data;
        let trace_id = params.trace_id;

        // ADR 011: Use spawn_blocking for heavy FSM I/O to preserve the tick loop
        tokio::task::spawn_blocking(move || {
            let span = info_span!(
                target: ClinicalTarget::RaftCompaction.as_str(),
                "background_snapshot_install",
                index = %index,
                trace_id = %trace_id
            );
            let _enter = span.enter();

            info!("Starting background snapshot installation...");

            let _fsm_guard = shell_clone.fsm_lock.blocking_lock();
            let res = fsm.install_snapshot(index, &data, trace_id);
            drop(_fsm_guard);

            // Phase 3: Lock & Finalize
            let mut guard = shell_clone.blocking_write();
            if let Err(e) = shell_clone.thaw() {
                guard.apply_fatal(NodeError::Protocol(format!(
                    "Structural Invariant Violation: {}",
                    e.0
                )));
            }

            match res {
                Ok(_) => {
                    info!(index = %index, "Background snapshot installation complete.");
                    guard.save_snapshot_metadata(index, term);

                    // ADR 011: Advance BOTH commit_index and volatile last_applied
                    // to ensure the next application loop starts after the snapshot.
                    if let Err(e) = guard.advance_horizon_after_snapshot(index) {
                        error!(index = %index, error = %e, "FATAL: Failed to advance logical horizon after snapshot.");
                        guard.apply_fatal(e);
                    }
                }
                Err(e) => {
                    error!(index = %index, error = %e, "FATAL: Snapshot installation failed.");
                    guard.apply_fatal(NodeError::Protocol(format!(
                        "Background snapshot restoration failure: {}",
                        e
                    )));
                }
            }
        });
    }

    Ok(current_term)
}

/// Performs a network-bound quorum check to verify leadership (§8).
///
/// Triggers an immediate heartbeat broadcast and awaits confirmation from
/// a majority of peers. Guarantees strict linearizability (ADR 006).
pub(crate) async fn verify_leadership_quorum<S: StateMachine>(
    shell: &Arc<ConsensusShell<S>>,
    config: Arc<Config>,
    peer_manager: Arc<PeerManager>,
    trace_id: TraceId,
) -> Result<(), ConsensusError> {
    // 1. Prepare the probe and record target epoch.
    // We use explicit health and role checks here to avoid the delegate_to_inner!
    // macro, which panics on poisoned nodes. This allows the API to return a
    // structured error instead of a process-wide panic (ADR 009).
    let (target_epoch, already_in_flight, term, node_id, last_committed) = {
        let mut guard = shell.write().await;

        if guard.is_poisoned() {
            return Err(ConsensusError::Poisoned);
        }

        let node_id = guard.node_id();
        let last_committed = guard.last_committed();

        if let Some(leader) = guard.as_leader_mut() {
            let term = leader.current_term().map_err(|_| {
                ConsensusError::Internal("Leadership verification failed".to_string())
            })?;
            let self_id = leader.node_id();
            let current = leader.state().current_read_epoch();
            let target = leader.state_mut().prepare_read_probe(self_id);
            (target, target == current, term, node_id, last_committed)
        } else {
            return Err(ConsensusError::NotLeader);
        }
    };

    // 2. Establish child span for mechanism visibility
    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        "quorum_probe",
        node_id = %node_id,
        trace_id = %trace_id,
        target_epoch = %target_epoch,
        term = %term
    );

    async {
        let mut progress_rx = shell.subscribe();
        let timeout_dur = config.raft.rpc_timeout();

        // 3. Trigger immediate replication (heartbeat broadcast) if a new round is
        //    needed.
        if !already_in_flight {
            initiate_replication(
                config,
                shell.clone(),
                peer_manager,
                ReplicationRoundParams {
                    term,
                    node_id,
                    last_committed,
                    trace_id,
                },
                span.clone(),
            );
        }

        // 4. Await quorum confirmation, demotion, or timeout.
        loop {
            // Check if already reached
            {
                let progress = progress_rx.borrow();
                if progress.role != NodeRole::Leader || progress.term != term {
                    return Err(ConsensusError::NotLeader);
                }
                if progress.confirmed_read_epoch >= target_epoch {
                    return Ok(());
                }
            }

            tokio::select! {
                Ok(_) = progress_rx.changed() => {
                    // Loop will check condition
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    return Err(ConsensusError::Timeout);
                }
            }
        }
    }
    .instrument(span.clone())
    .await
}
