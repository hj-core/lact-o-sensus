//! gRPC message definitions and NewType-aware factories for Lact-O-Sensus.
//!
//! This module provides the specialized implementation of gRPC message types
//! generated from `raft.proto` and `app.proto`. Following Rule 4, all messages
//! must be instantiated via these factories to ensure that domain-specific
//! NewTypes are correctly converted to wire-safe primitives and that
//! architectural invariants are maintained at the boundary.

/// Version 1 of the Lact-O-Sensus protocol.
pub mod v1 {
    /// Raft consensus protocol messages.
    pub mod raft {
        tonic::include_proto!("raft.v1");

        use crate::types::LogIndex;
        use crate::types::NodeId;
        use crate::types::Term;

        impl LogEntry {
            /// Constructs a new LogEntry with proper NewType conversion.
            pub fn new(index: LogIndex, term: Term, data: Vec<u8>) -> Self {
                Self {
                    index: index.as_u64(),
                    term: term.as_u64(),
                    data,
                }
            }
        }

        impl RequestVoteRequest {
            /// Constructs a new RequestVoteRequest with proper NewType
            /// conversion.
            pub fn new(
                term: Term,
                candidate_id: NodeId,
                last_log_index: LogIndex,
                last_log_term: Term,
            ) -> Self {
                Self {
                    term: term.as_u64(),
                    candidate_id: candidate_id.to_string(),
                    last_log_index: last_log_index.as_u64(),
                    last_log_term: last_log_term.as_u64(),
                }
            }
        }

        impl RequestVoteResponse {
            /// Constructs a new RequestVoteResponse with proper NewType
            /// conversion.
            pub fn new(term: Term, vote_granted: bool) -> Self {
                Self {
                    term: term.as_u64(),
                    vote_granted,
                }
            }
        }

        impl AppendEntriesRequest {
            /// Constructs a new AppendEntriesRequest with proper NewType
            /// conversion.
            pub fn new(
                term: Term,
                leader_id: NodeId,
                prev_log_index: LogIndex,
                prev_log_term: Term,
                entries: Vec<LogEntry>,
                leader_commit: LogIndex,
            ) -> Self {
                Self {
                    term: term.as_u64(),
                    leader_id: leader_id.to_string(),
                    prev_log_index: prev_log_index.as_u64(),
                    prev_log_term: prev_log_term.as_u64(),
                    entries,
                    leader_commit: leader_commit.as_u64(),
                }
            }
        }

        impl AppendEntriesResponse {
            /// Constructs a new AppendEntriesResponse with proper NewType
            /// conversion.
            pub fn new(term: Term, success: bool, last_log_index: LogIndex) -> Self {
                Self {
                    term: term.as_u64(),
                    success,
                    last_log_index: last_log_index.as_u64(),
                }
            }
        }
    }

    /// Lact-O-Sensus application-level messages.
    pub mod app {
        tonic::include_proto!("lacto_sensus.v1");

        use ::prost_types::Timestamp;

        use crate::types::ClientId;
        use crate::types::LogIndex;
        use crate::types::SequenceId;

        impl GroceryItem {
            /// Constructs a new GroceryItem with full fidelity and NewType
            /// conversion.
            #[allow(clippy::too_many_arguments)]
            pub fn new(
                item_key: String,
                quantity: String,
                unit: String,
                category: String,
                last_modifier_id: String,
                last_activity: Timestamp,
                state_version: LogIndex,
            ) -> Self {
                Self {
                    item_key,
                    quantity,
                    unit,
                    category,
                    last_modifier_id,
                    last_activity: Some(last_activity),
                    state_version: state_version.as_u64(),
                }
            }
        }

        impl MutationIntent {
            /// Constructs a new MutationIntent with optional fields and enum
            /// conversion.
            pub fn new(
                item_key: String,
                quantity: Option<String>,
                unit: Option<String>,
                category: Option<String>,
                operation: OperationType,
            ) -> Self {
                Self {
                    item_key,
                    quantity,
                    unit,
                    category,
                    operation: operation as i32,
                }
            }
        }

        impl QueryStateRequest {
            /// Constructs a new QueryStateRequest with optional NewType
            /// conversion.
            pub fn new(query_filter: Option<String>, min_state_version: Option<LogIndex>) -> Self {
                Self {
                    query_filter,
                    min_state_version: min_state_version.map(|v| v.as_u64()),
                }
            }
        }

        impl EvaluateProposalRequest {
            /// Constructs a new EvaluateProposalRequest with ClientId fidelity.
            pub fn new(
                client_id: &ClientId,
                intent: MutationIntent,
                current_inventory: Vec<GroceryItem>,
                request_context: String,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    intent: Some(intent),
                    current_inventory,
                    request_context,
                }
            }
        }

        impl ProposeMutationRequest {
            /// Constructs a new ProposeMutationRequest with SequenceId
            /// fidelity.
            pub fn new(
                client_id: &ClientId,
                sequence_id: SequenceId,
                intent: MutationIntent,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    sequence_id: sequence_id.as_u64(),
                    intent: Some(intent),
                }
            }
        }

        impl QueryStateResponse {
            /// Constructs a new QueryStateResponse with state version fidelity.
            pub fn new(
                items: Vec<GroceryItem>,
                current_state_version: LogIndex,
                status: QueryStatus,
                leader_hint: String,
                error_message: String,
            ) -> Self {
                Self {
                    items,
                    current_state_version: current_state_version.as_u64(),
                    status: status as i32,
                    leader_hint,
                    error_message,
                }
            }
        }

        impl ProposeMutationResponse {
            /// Constructs a new ProposeMutationResponse with state version
            /// fidelity.
            pub fn new(
                status: MutationStatus,
                state_version: LogIndex,
                leader_hint: String,
                error_message: String,
            ) -> Self {
                Self {
                    status: status as i32,
                    state_version: state_version.as_u64(),
                    leader_hint,
                    error_message,
                }
            }
        }

        impl SessionRecord {
            /// Constructs a new SessionRecord with EOS and timing fidelity.
            pub fn new(
                client_id: &ClientId,
                last_sequence_id: SequenceId,
                status: MutationStatus,
                log_index: LogIndex,
                moral_justification: String,
                last_activity_effective_time: Timestamp,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    last_sequence_id: last_sequence_id.as_u64(),
                    status: status as i32,
                    log_index: log_index.as_u64(),
                    moral_justification,
                    last_activity_effective_time: Some(last_activity_effective_time),
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
                status: MutationStatus,
                now: std::time::SystemTime,
            ) -> Self {
                Self {
                    client_id: client_id.as_str().to_string(),
                    sequence_id: sequence_id.as_u64(),
                    resolved_item_key,
                    suggested_display_name,
                    updated_base_quantity,
                    base_unit,
                    display_unit,
                    updated_category,
                    raw_user_input,
                    moral_justification,
                    is_delete,
                    status: status as i32,
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
    use super::v1::raft::*;
    use crate::types::ClientId;
    use crate::types::LogIndex;
    use crate::types::NodeId;
    use crate::types::SequenceId;
    use crate::types::Term;

    mod raft {
        use super::*;

        mod log_entry {
            use super::*;
            mod new {
                use super::*;
                #[test]
                fn initializes_fields_with_correct_conversions() {
                    let entry = LogEntry::new(LogIndex::new(1), Term::new(2), vec![1, 2, 3]);
                    assert_eq!(entry.index, 1);
                    assert_eq!(entry.term, 2);
                    assert_eq!(entry.data, vec![1, 2, 3]);
                }
            }
        }

        mod request_vote_request {
            use super::*;
            mod new {
                use super::*;
                #[test]
                fn initializes_fields_with_correct_conversions() {
                    let req = RequestVoteRequest::new(
                        Term::new(5),
                        NodeId::try_new(1).unwrap(),
                        LogIndex::new(10),
                        Term::new(4),
                    );
                    assert_eq!(req.term, 5);
                    assert_eq!(req.candidate_id, "1");
                    assert_eq!(req.last_log_index, 10);
                    assert_eq!(req.last_log_term, 4);
                }
            }
        }
    }

    mod app {
        use prost_types::Timestamp;

        use super::*;

        mod grocery_item {
            use super::*;
            mod new {
                use super::*;
                #[test]
                fn correctly_maps_log_index_to_u64_when_created() {
                    let index = LogIndex::new(100);
                    let ts = Timestamp {
                        seconds: 123,
                        nanos: 456,
                    };
                    let item = GroceryItem::new(
                        "key".into(),
                        "1".into(),
                        "unit".into(),
                        "cat".into(),
                        "mod".into(),
                        ts,
                        index,
                    );

                    assert_eq!(item.state_version, 100);
                    assert_eq!(item.last_activity.unwrap().seconds, 123);
                }
            }
        }

        mod mutation_intent {
            use super::*;
            mod new {
                use super::*;
                #[test]
                fn handles_optional_fields_correctly() {
                    let intent = MutationIntent::new(
                        "apple".to_string(),
                        Some("5".to_string()),
                        None,
                        None,
                        OperationType::Add,
                    );

                    assert_eq!(intent.item_key, "apple");
                    assert_eq!(intent.quantity, Some("5".to_string()));
                    assert!(intent.unit.is_none());
                    assert!(intent.category.is_none());
                    assert_eq!(intent.operation, OperationType::Add as i32);
                }
            }
        }

        mod query_state_request {
            use super::*;
            mod new {
                use super::*;
                #[test]
                fn maps_optional_log_index_correctly_when_present() {
                    let index = LogIndex::new(42);
                    let req = QueryStateRequest::new(None, Some(index));
                    assert_eq!(req.min_state_version, Some(42));
                }

                #[test]
                fn handles_none_for_all_optional_fields() {
                    let req = QueryStateRequest::new(None, None);
                    assert!(req.query_filter.is_none());
                    assert!(req.min_state_version.is_none());
                }
            }
        }

        mod evaluate_proposal_response {
            use super::*;
            mod new {
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
                        MutationStatus::Committed,
                        now,
                    );

                    let mut buf = Vec::new();
                    original.encode(&mut buf).expect("Failed to encode");
                    let decoded = CommittedMutation::decode(&buf[..]).expect("Failed to decode");

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
    }
}
