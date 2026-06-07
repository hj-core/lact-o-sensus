use std::time::Duration;
use std::time::SystemTime;

use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::OperationType;
use common::proto::v1::app::ProposeMutationResponse;
use common::types::LogIndex;
use common::types::errors::ConsensusError;
use common::types::trace::ClinicalTarget;
use prost::Message;
use raft_engine::ConsensusAuthority;
use raft_engine::ConsensusHandle;
use tonic::Response;
use tonic::Status;
use tracing::error;
use tracing::info;

use super::MutationProposal;

/// Commits the finalized intent to the consensus log and waits for quorum
/// commitment.
pub(crate) async fn commit_to_consensus(
    raft_handle: &dyn ConsensusHandle,
    consensus_timeout: Duration,
    proposal: MutationProposal<'_>,
) -> Result<LogIndex, Status> {
    let is_delete = proposal.intent.operation == OperationType::Delete as i32;

    let mutation = CommittedMutation::new(
        proposal.client_id,
        proposal.sequence_id,
        proposal.stabilized.resolved_item_key,
        proposal.stabilized.suggested_display_name,
        proposal.stabilized.updated_base_quantity,
        proposal.stabilized.base_unit,
        proposal.stabilized.display_unit,
        proposal.stabilized.category.to_string(),
        proposal.raw_user_input,
        proposal.stabilized.moral_justification,
        is_delete,
        proposal.status,
        SystemTime::now(),
    );

    let mut command = Vec::new();
    mutation
        .encode(&mut command)
        .map_err(|e| Status::internal(e.to_string()))?;

    let proposal_index = raft_handle
        .propose(command)
        .await
        .map_err(|e| map_consensus_error(e, proposal.consensus_status))?;

    info!(
        "Mutation index {} appended. Waiting for quorum...",
        proposal_index
    );

    tokio::time::timeout(consensus_timeout, raft_handle.await_commit(proposal_index))
        .await
        .map_err(|_| {
            Status::deadline_exceeded(format!(
                "Quorum commitment for index {} timed out after {:?}",
                proposal_index, consensus_timeout
            ))
        })?
        .map_err(|e| map_consensus_error(e, proposal.consensus_status))?;
    Ok(proposal_index)
}

/// Constructs the final gRPC response reflecting the committed lifecycle
/// status.
pub(crate) fn build_mutation_response(
    index: LogIndex,
    status: MutationStatus,
    moral_justification: String,
) -> Response<ProposeMutationResponse> {
    Response::new(ProposeMutationResponse {
        status: status as i32,
        state_version: index.as_u64(),
        leader_hint: String::new(),
        error_message: if status == MutationStatus::Vetoed {
            moral_justification
        } else {
            String::new()
        },
    })
}

/// Translates domain ConsensusErrors into standard gRPC Status objects.
pub(crate) fn map_consensus_error(err: ConsensusError, status: &ConsensusAuthority) -> Status {
    match err {
        ConsensusError::NotLeader => Status::failed_precondition(format!(
            "Not the leader. Hint: {} ({})",
            status.leader_hint, status.rejection_reason
        )),
        ConsensusError::LeaderUnknown => {
            Status::unavailable("Leader unknown. Election in progress.")
        }
        ConsensusError::CommitTimeout(idx) => {
            Status::deadline_exceeded(format!("Proposal at index {} timed out", idx))
        }
        ConsensusError::Poisoned => {
            error!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                "CRITICAL: Node is in a poisoned state due to fatal safety violation."
            );
            Status::aborted("Node is in a fatal state and cannot process requests.")
        }
        ConsensusError::Internal(msg) => {
            error!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                error = %msg,
                "Consensus Internal Error"
            );
            Status::internal("Internal Consensus Failure")
        }
        ConsensusError::Terminated => Status::unavailable("Consensus engine is shutting down"),
        ConsensusError::Timeout => Status::deadline_exceeded("Quorum verification timed out"),
    }
}
