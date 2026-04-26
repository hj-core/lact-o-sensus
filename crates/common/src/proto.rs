pub mod v1 {
    pub mod raft {
        tonic::include_proto!("raft.v1");

        use crate::types::LogIndex;
        use crate::types::NodeId;
        use crate::types::Term;

        impl LogEntry {
            pub fn new(index: LogIndex, term: Term, data: Vec<u8>) -> Self {
                Self {
                    index: index.value(),
                    term: term.value(),
                    data,
                }
            }
        }

        impl RequestVoteRequest {
            pub fn new(
                term: Term,
                candidate_id: NodeId,
                last_log_index: LogIndex,
                last_log_term: Term,
            ) -> Self {
                Self {
                    term: term.value(),
                    candidate_id: candidate_id.to_string(),
                    last_log_index: last_log_index.value(),
                    last_log_term: last_log_term.value(),
                }
            }
        }

        impl RequestVoteResponse {
            pub fn new(term: Term, vote_granted: bool) -> Self {
                Self {
                    term: term.value(),
                    vote_granted,
                }
            }
        }

        impl AppendEntriesRequest {
            pub fn new(
                term: Term,
                leader_id: NodeId,
                prev_log_index: LogIndex,
                prev_log_term: Term,
                entries: Vec<LogEntry>,
                leader_commit: LogIndex,
            ) -> Self {
                Self {
                    term: term.value(),
                    leader_id: leader_id.to_string(),
                    prev_log_index: prev_log_index.value(),
                    prev_log_term: prev_log_term.value(),
                    entries,
                    leader_commit: leader_commit.value(),
                }
            }
        }

        impl AppendEntriesResponse {
            pub fn new(term: Term, success: bool, last_log_index: LogIndex) -> Self {
                Self {
                    term: term.value(),
                    success,
                    last_log_index: last_log_index.value(),
                }
            }
        }
    }

    pub mod app {
        tonic::include_proto!("lacto_sensus.v1");

        use ::prost_types::Timestamp;

        use crate::types::ClientId;
        use crate::types::LogIndex;
        use crate::types::SequenceId;

        impl ProposeMutationRequest {
            pub fn new(
                client_id: &ClientId,
                sequence_id: SequenceId,
                intent: MutationIntent,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    sequence_id: sequence_id.value(),
                    intent: Some(intent),
                }
            }
        }

        impl QueryStateResponse {
            pub fn new(
                items: Vec<GroceryItem>,
                current_state_version: LogIndex,
                status: QueryStatus,
                leader_hint: String,
                error_message: String,
            ) -> Self {
                Self {
                    items,
                    current_state_version: current_state_version.value(),
                    status: status as i32,
                    leader_hint,
                    error_message,
                }
            }
        }

        impl ProposeMutationResponse {
            pub fn new(
                status: MutationStatus,
                state_version: LogIndex,
                leader_hint: String,
                error_message: String,
            ) -> Self {
                Self {
                    status: status as i32,
                    state_version: state_version.value(),
                    leader_hint,
                    error_message,
                }
            }
        }

        impl EvaluateProposalResponse {
            /// Creates a new AI evaluation response with full semantic
            /// resolution.
            #[allow(clippy::too_many_arguments)]
            pub fn new(
                is_approved: bool,
                category_assignment: String,
                moral_justification: String,
                resolved_item_key: String,
                suggested_display_name: String,
                resolved_unit: String,
                conversion_multiplier_to_base: String,
            ) -> Self {
                Self {
                    is_approved,
                    category_assignment,
                    moral_justification,
                    resolved_item_key,
                    suggested_display_name,
                    resolved_unit,
                    conversion_multiplier_to_base,
                }
            }
        }

        impl CommittedMutation {
            /// Creates a new finalized mutation record with absolute values
            /// and AI-vetted metadata (ADR 005).
            #[allow(clippy::too_many_arguments)]
            pub fn new(
                client_id: &ClientId,
                sequence_id: SequenceId,
                resolved_item_key: String,
                suggested_display_name: String,
                updated_base_quantity: String,
                base_unit: String,
                display_unit: String,
                updated_category: String,
                raw_user_input: String,
                moral_justification: String,
                is_delete: bool,
                now: std::time::SystemTime,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    sequence_id: sequence_id.value(),
                    resolved_item_key,
                    suggested_display_name,
                    updated_base_quantity,
                    base_unit,
                    display_unit,
                    updated_category,
                    raw_user_input,
                    moral_justification,
                    is_delete,
                    event_time: Some(Timestamp::from(now)),
                }
            }
        }
    }

    // Re-export for backward compatibility and convenience
    pub use app::*;
    pub use raft::*;
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use prost::Message;

    use super::v1::app::*;
    use crate::types::ClientId;
    use crate::types::SequenceId;

    mod committed_mutation {
        use super::*;

        mod serialization {
            use super::*;

            #[test]
            fn supports_binary_round_trip() {
                let cid = ClientId::generate();
                let sid = SequenceId::new(42);
                let now = SystemTime::now();

                let original = CommittedMutation::new(
                    &cid,
                    sid,
                    "milk-whole".to_string(),
                    "Whole Milk".to_string(),
                    "2000".to_string(),
                    "ml".to_string(),
                    "L".to_string(),
                    "Dairy".to_string(),
                    "add 2L milk".to_string(),
                    "Valid dairy item".to_string(),
                    false,
                    now,
                );

                // 1. Serialize to buffer
                let mut buf = Vec::new();
                original.encode(&mut buf).expect("Failed to encode");

                // 2. Deserialize from buffer
                let decoded = CommittedMutation::decode(&buf[..]).expect("Failed to decode");

                // 3. Verify parity
                assert_eq!(original.client_id, decoded.client_id);
                assert_eq!(original.sequence_id, decoded.sequence_id);
                assert_eq!(original.resolved_item_key, decoded.resolved_item_key);
                assert_eq!(
                    original.updated_base_quantity,
                    decoded.updated_base_quantity
                );
                assert_eq!(original.moral_justification, decoded.moral_justification);
                assert_eq!(original.is_delete, decoded.is_delete);
                assert!(decoded.event_time.is_some());
            }
        }
    }

    mod evaluate_proposal_response {
        use super::*;

        mod instantiation {
            use super::*;

            #[test]
            fn initializes_full_semantic_resolution_metadata() {
                let resp = EvaluateProposalResponse::new(
                    true,
                    "Dairy".to_string(),
                    "Justified".to_string(),
                    "milk-slug".to_string(),
                    "Milk".to_string(),
                    "ml".to_string(),
                    "1000.0".to_string(),
                );

                assert!(resp.is_approved);
                assert_eq!(resp.category_assignment, "Dairy");
                assert_eq!(resp.resolved_item_key, "milk-slug");
                assert_eq!(resp.conversion_multiplier_to_base, "1000.0");
            }
        }
    }
}
