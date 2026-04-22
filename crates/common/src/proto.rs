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

        impl CommittedMutation {
            /// Creates a new finalized mutation record with absolute values.
            /// Supply direct fields to allow the Leader to resolve deltas/AI
            /// categories before log entry creation.
            pub fn new(
                client_id: &ClientId,
                sequence_id: SequenceId,
                item_key: String,
                updated_quantity: String,
                updated_category: String,
                updated_unit: String,
                is_delete: bool,
                now: std::time::SystemTime,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    sequence_id: sequence_id.value(),
                    item_key,
                    updated_quantity,
                    updated_category,
                    updated_unit,
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
