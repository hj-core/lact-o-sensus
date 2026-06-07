//! Consensus RPC Utilities
//!
//! Houses the building blocks for constructing, dispatching, and verifying
//! Raft RPC messages, including replication strategy selection, request
//! construction, commit advancement, and causal trace integrity verification
//! (ADR 010).
//!
//! All functions are `pub(super)` and accessed by sibling submodules via
//! private `use` re-exports in `mod.rs`.

use common::proto::v1::raft::AppendEntriesRequest;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use common_rpc::TraceInterceptor;
use tonic::Status;
use tracing::info;
use tracing::warn;

use super::types::ReplicationRoundParams;
use super::types::ReplicationStrategy;
use super::types::RpcResult;
use crate::engine::LogicalNode;
use crate::engine::RoleState;

// --- Telemetry & Identity ---

/// Maps the physical node state to a semantic role name for telemetry spans.
pub(super) fn determine_node_role_name<S: StateMachine>(node: &LogicalNode<S>) -> &'static str {
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
pub(super) fn determine_replication_strategy<S: StateMachine>(
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
pub(super) fn build_append_entries_request<S: StateMachine>(
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
pub(super) fn update_leader_last_committed<S: StateMachine>(node: &mut LogicalNode<S>) {
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
pub(super) fn verify_trace_integrity<T>(
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
