use common::app_api::SessionProvider;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::ProposeMutationResponse;
use common::types::ClientId;
use common::types::SequenceId;
use common::types::trace::ClinicalTarget;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::warn;

/// Enforces Exactly-Once Semantics by validating request sequences against
/// the authoritative session table.
///
/// Returns a cached response if the request is a duplicate, or a logical
/// rejection if it violates session continuity.
pub(crate) async fn enforce_sequence_firewall(
    session_provider: &dyn SessionProvider,
    client_id: &ClientId,
    sequence_id: SequenceId,
) -> Result<Option<Response<ProposeMutationResponse>>, Status> {
    if sequence_id.as_u64() == 0 {
        return Ok(Some(Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint: String::new(),
            error_message: "Secure Clinical: Protocol Violation (Sequence ID 0)".to_string(),
        })));
    }

    let last_session = session_provider
        .check_session(client_id, SequenceId::new(0))
        .map_err(|e| Status::invalid_argument(format!("Session lookup failed: {}", e)))?;

    if let Some(record) = last_session {
        if sequence_id.as_u64() == record.last_sequence_id {
            info!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                client_id = %client_id.truncated(),
                seq = %sequence_id,
                "Deduplicating request. Returning cached outcome."
            );
            return Ok(Some(Response::new(ProposeMutationResponse {
                status: record.status,
                state_version: record.log_index,
                leader_hint: String::new(),
                error_message: if record.status == MutationStatus::Vetoed as i32 {
                    record.moral_justification
                } else {
                    String::new()
                },
            })));
        }

        if sequence_id.as_u64() < record.last_sequence_id {
            warn!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                client_id = %client_id.truncated(),
                seq = %sequence_id,
                cluster_seq = %record.last_sequence_id,
                "Rejecting stale request."
            );
            return Ok(Some(Response::new(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                state_version: record.log_index,
                leader_hint: String::new(),
                error_message: "Secure Clinical: Stale Sequence".to_string(),
            })));
        }

        if sequence_id.as_u64() > record.last_sequence_id + 1 {
            warn!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                client_id = %client_id.truncated(),
                seq = %sequence_id,
                expected_seq = %(record.last_sequence_id + 1),
                "Rejecting sequence gap."
            );
            return Ok(Some(Response::new(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                state_version: 0,
                leader_hint: String::new(),
                error_message: "Secure Clinical: Sequence Continuity Violation".to_string(),
            })));
        }
    } else if sequence_id.as_u64() != 1 {
        // New client must start with sequence 1
        warn!(
            target: ClinicalTarget::ClinicalIngress.as_str(),
            client_id = %client_id.truncated(),
            seq = %sequence_id,
            expected_seq = 1,
            "Rejecting session bootstrap gap."
        );
        return Ok(Some(Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint: String::new(),
            error_message: "Secure Clinical: Session Initialization Violation".to_string(),
        })));
    }

    Ok(None)
}
