use std::time::Duration;

use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::taxonomy::GroceryCategory;
use common::types::ClientId;
use common::types::SequenceId;
use raft_engine::ConsensusAuthority;
use tonic::Response;
use tonic::Status;

/// Configuration for the Ingress Layer.
#[derive(Debug, Clone)]
pub struct IngressConfig {
    pub veto_timeout: Duration,
    pub consensus_timeout: Duration,
    /// Maximum number of leader-internal retries for AI resolution.
    pub veto_max_retries: usize,
    /// Maximum characters allowed in the AI's moral justification.
    pub max_justification_len: usize,
}

/// Validated and mathematically stabilized data ready for consensus.
///
/// Implements Layer 4 (Validation Proxy) of the Defensive Onion (ADR 007).
#[derive(Debug, Clone)]
pub(crate) struct StabilizedMutation {
    pub(crate) resolved_item_key: String,
    pub(crate) suggested_display_name: String,
    pub(crate) updated_base_quantity: String,
    pub(crate) base_unit: String,
    pub(crate) display_unit: String,
    pub(crate) category: GroceryCategory,
    pub(crate) moral_justification: String,
}

/// Encapsulated data for a mutation proposal to be committed to consensus.
#[derive(Debug)]
pub(crate) struct MutationProposal<'a> {
    pub(crate) client_id: &'a ClientId,
    pub(crate) sequence_id: SequenceId,
    pub(crate) intent: MutationIntent,
    pub(crate) stabilized: StabilizedMutation,
    pub(crate) raw_user_input: String,
    pub(crate) status: MutationStatus,
    pub(crate) consensus_status: &'a ConsensusAuthority,
}

/// Outcome of a clinical authority verification check.
pub(crate) enum AuthorityOutcome<R> {
    /// Node is healthy and authorized as the leader.
    Authorized(ConsensusAuthority),
    /// Node is healthy but not the leader; request should be redirected.
    Redirect(Response<R>),
    /// Node is poisoned; request must fail immediately (Halt Mandate).
    Fatal(Status),
}
