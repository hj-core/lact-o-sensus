pub mod v1 {
    tonic::include_proto!("lacto_sensus.v1");

    // We mapped .google.protobuf.Timestamp to ::prost_types::Timestamp in build.rs
    use ::prost_types::Timestamp;

    impl CommittedMutation {
        /// Creates a new finalized mutation record with absolute values.
        /// Supply direct fields to allow the Leader to resolve deltas/AI
        /// categories before log entry creation.
        pub fn new(
            client_id: String,
            sequence_id: u64,
            item_key: String,
            updated_quantity: String,
            updated_category: String,
            updated_unit: String,
            is_delete: bool,
            now: std::time::SystemTime,
        ) -> Self {
            Self {
                client_id,
                sequence_id,
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
