use std::time::Duration;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::InstallSnapshotResponse;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::TraceId;
use tonic::Status;

use crate::shell::SnapshotPermit;

/// Multiplier applied to the base RPC timeout to allow for connection
/// establishment (TCP/TLS/HTTP2 handshakes) on cold starts (Rule [ENG-03]).
pub(super) const CONNECTION_CUSHION_MULTIPLIER: u32 = 4;

/// Standardized result type for internal Raft RPC operations.
///
/// Distinguishes transient network or protocol errors (Status) from
/// terminal system-level failures (NodeError).
pub(super) type RpcResult<T> = Result<T, Status>;

/// Standardized result type for consensus orchestration logic.
pub(super) type ConsensusResult<T> = Result<T, NodeError>;

// --- 1. The Election Cycle ---

/// Consolidated parameters for a RequestVote RPC.
///
/// Bundles Raft coordinates with telemetry context to ensure causal
/// verification during leadership campaigns (ADR 010).
#[derive(Debug, Clone, Copy)]
pub(super) struct VoteRequestParams {
    pub(super) term: Term,
    pub(super) node_id: NodeId,
    pub(super) last_log_index: LogIndex,
    pub(super) last_log_term: Term,
    pub(super) rpc_timeout: Duration,
    pub(super) trace_id: TraceId,
}

/// DTO for Election Campaign parameters, captured during the atomic tick
/// boundary.
///
/// Ensures the asynchronous campaign task has a consistent snapshot of the
/// node's identity and log coordinates at the moment the election was
/// triggered.
#[derive(Debug, Clone, Copy)]
pub(super) struct ElectionCampaignParams {
    pub(super) term: Term,
    pub(super) node_id: NodeId,
    pub(super) last_log_index: LogIndex,
    pub(super) last_log_term: Term,
    pub(super) trace_id: TraceId,
}

/// DTO for PreVote Campaign parameters, captured during the tick boundary.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreVoteCampaignParams {
    pub(crate) term: Term,
    pub(crate) node_id: NodeId,
    pub(crate) last_log_index: LogIndex,
    pub(crate) last_log_term: Term,
    pub(crate) rpc_timeout: std::time::Duration,
    pub(crate) trace_id: TraceId,
}

/// Decision outcomes from the pre-vote tallying process.
#[derive(Debug, PartialEq)]
pub(super) enum PreVoteAction {
    /// Pre-vote quorum reached; transition to Candidate for real election.
    PreVoteQuorumReached,
    /// Pre-vote denied; return to Follower without term change.
    PreVoteDenied,
    /// Pre-vote campaign continues.
    Continue,
}

/// Decision outcomes from the vote-tallying process.
///
/// Maps the distributed responses from peers into immediate state
/// transitions for the Candidate.
#[derive(Debug, PartialEq)]
pub(super) enum VoteAction {
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
pub(super) enum ReplicationOutcome<S: StateMachine> {
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
pub(super) enum ReplicationAction {
    /// Node has been demoted to Follower due to a higher term (§5.1).
    Demoted,
    /// Replication continues for other peers.
    Continue,
}

/// Logical strategy for a single peer replication attempt.
pub(super) enum ReplicationStrategy {
    /// Follower is within the log horizon; send incremental updates.
    AppendEntries(AppendEntriesRequest),
    /// Follower is behind the horizon; send the full state machine.
    InstallSnapshot {
        last_included_index: LogIndex,
        last_included_term: Term,
    },
}
