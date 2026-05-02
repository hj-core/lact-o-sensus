use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::OperationType;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_server::IngressService;
use common::raft_api::ConsensusStatus;
use common::raft_api::RaftHandle;
use common::taxonomy::GroceryCategory;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::units::PhysicalQuantity;
use common::units::UnitRegistry;
use prost::Message;
use rust_decimal::Decimal;
use tokio::sync::Mutex;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;

use crate::veto::VetoError;
use crate::veto::VetoOutcome;
use crate::veto::VetoRelay;

/// Trait for fetching the current state of the grocery inventory.
#[async_trait]
pub trait InventorySource: Send + Sync + std::fmt::Debug {
    /// Returns the current list of items in the inventory.
    async fn get_inventory(&self) -> Vec<GroceryItem>;
}

/// Validated and mathematically stabilized data ready for consensus.
///
/// Implements Layer 4 (Validation Proxy) of the Defensive Onion (ADR 007).
#[derive(Debug, Clone)]
struct StabilizedMutation {
    resolved_item_key: String,
    suggested_display_name: String,
    updated_base_quantity: String,
    base_unit: String,
    display_unit: String,
    category: GroceryCategory,
    moral_justification: String,
}

/// Implementation of the external client ingress RPCs.
///
/// This service handles user mutations and state queries, enforcing
/// cluster identity and redirecting clients to the current leader.
#[derive(Debug)]
pub struct IngressDispatcher {
    raft_handle: Arc<dyn RaftHandle>,
    inventory_source: Arc<dyn InventorySource>,
    veto_relay: Arc<dyn VetoRelay>,
    veto_timeout: Duration,
    /// Maximum number of leader-internal retries for AI resolution.
    veto_max_retries: usize,
    /// Maximum characters allowed in the AI's moral justification.
    max_justification_len: usize,
    /// Mutex serving as the Layer 2 MutationLock (ADR 007).
    /// Ensures that AI evaluation and proposal happen sequentially on the
    /// leader.
    mutation_lock: Mutex<()>,
}

impl IngressDispatcher {
    /// Creates a new IngressDispatcher with configured AI policy parameters.
    pub fn new(
        raft_handle: Arc<dyn RaftHandle>,
        inventory_source: Arc<dyn InventorySource>,
        veto_relay: Arc<dyn VetoRelay>,
        veto_timeout: Duration,
        veto_max_retries: usize,
        max_justification_len: usize,
    ) -> Self {
        Self {
            raft_handle,
            inventory_source,
            veto_relay,
            veto_timeout,
            veto_max_retries,
            max_justification_len,
            mutation_lock: Mutex::new(()),
        }
    }

    /// Acquires the Layer 2 MutationLock.
    ///
    /// This ensures that AI evaluation and proposal happen sequentially,
    /// providing the AI with a stable view of the inventory.
    async fn acquire_mutation_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation_lock.lock().await
    }

    /// Helper to construct a standard "Rejected/Redirection" response from a
    /// status snapshot.
    fn rejection_response_with_status(
        &self,
        status: ConsensusStatus,
    ) -> Response<ProposeMutationResponse> {
        Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint: status.leader_hint,
            error_message: status.rejection_reason,
        })
    }

    /// Returns a standard gRPC InvalidArgument status.
    fn invalid_argument(&self, msg: impl Into<String>) -> Status {
        Status::invalid_argument(msg)
    }

    /// Returns a standard gRPC Internal status.
    fn internal_error(&self, msg: impl Into<String>) -> Status {
        Status::internal(msg)
    }

    /// Normalizes and validates the user's intent before it is processed by
    /// the AI or added to the consensus log.
    ///
    /// Implements Layer 2 (Syntactic Scrubbing & Taxonomy Guard).
    fn normalize_intent(&self, intent: &mut MutationIntent) -> Result<(), Status> {
        intent.item_key = intent.item_key.trim().to_lowercase();

        if let Some(q) = intent.quantity.as_mut() {
            let trimmed = q.trim();
            if trimmed.is_empty() {
                intent.quantity = None;
            } else {
                let val = Decimal::from_str(trimmed).map_err(|_| {
                    self.invalid_argument(format!("Invalid quantity format: '{}'", trimmed))
                })?;
                if val.is_sign_negative() {
                    return Err(self.invalid_argument(
                        "quantity cannot be negative. Use SUBTRACT or DELETE for removals.",
                    ));
                }
                *q = trimmed.to_string();
            }
        }

        if let Some(unit) = intent.unit.as_mut() {
            *unit = unit.trim().to_lowercase();
        }

        // --- Taxonomy Guard (ADR 007 Layer 2) ---
        if let Some(category) = intent.category.as_mut() {
            let trimmed = category.trim();
            if !trimmed.is_empty() {
                // Verify hint against the 12-Point Authorized Taxonomy
                GroceryCategory::from_str(trimmed).map_err(|_| {
                    self.invalid_argument(format!(
                        "Invalid category hint: '{}'. Must be one of the 12 clinical categories.",
                        trimmed
                    ))
                })?;
                *category = trimmed.to_string();
            }
        }

        if intent.item_key.is_empty() {
            return Err(self.invalid_argument("item_key cannot be empty"));
        }

        // Validate quantity requirement based on operation type
        if intent.operation != OperationType::Delete as i32 && intent.quantity.is_none() {
            return Err(self.invalid_argument("quantity is required for this operation"));
        }

        Ok(())
    }

    /// Validates AI-provided metadata against system registries and calculates
    /// the stabilized SI base quantity.
    ///
    /// Implements Layer 4 (Validation Proxy) of the Defensive Onion (ADR 007).
    fn validate_and_stabilize(
        &self,
        intent: &MutationIntent,
        veto: &VetoOutcome,
        current_inventory: &[common::proto::v1::app::GroceryItem],
    ) -> Result<StabilizedMutation, Status> {
        let category = self.verify_category_registry(&veto.category_assignment)?;

        // For DELETE operations, we bypass physical stabilization
        if intent.operation == OperationType::Delete as i32 {
            return Ok(StabilizedMutation {
                resolved_item_key: veto.resolved_item_key.clone(),
                suggested_display_name: veto.suggested_display_name.clone(),
                updated_base_quantity: "0".to_string(),
                base_unit: "units".to_string(), // Placeholder for non-physical delete
                display_unit: veto.resolved_unit.clone(),
                category,
                moral_justification: veto.moral_justification.clone(),
            });
        }

        let q_str = intent
            .quantity
            .as_deref()
            .ok_or_else(|| self.invalid_argument("quantity is missing"))?;

        let base_quantity = self.verify_unit_stabilization(
            q_str,
            &veto.resolved_unit,
            &veto.conversion_multiplier_to_base,
        )?;

        self.enforce_physical_invariants(
            intent,
            &veto.resolved_item_key,
            &base_quantity,
            current_inventory,
        )?;

        Ok(StabilizedMutation {
            resolved_item_key: veto.resolved_item_key.clone(),
            suggested_display_name: veto.suggested_display_name.clone(),
            updated_base_quantity: base_quantity.value().to_string(),
            base_unit: base_quantity.dimension().base_unit().to_string(),
            display_unit: veto.resolved_unit.clone(),
            category,
            moral_justification: veto.moral_justification.clone(),
        })
    }

    /// Registry Firewall: Verifies the AI's category assignment.
    fn verify_category_registry(&self, category_str: &str) -> Result<GroceryCategory, Status> {
        GroceryCategory::from_str(category_str).map_err(|_| {
            self.internal_error(format!(
                "AI Hallucination: Unregistered category '{}'",
                category_str
            ))
        })
    }

    /// Registry Firewall & SI Math: Verifies unit existence and stabilizes
    /// quantity.
    fn verify_unit_stabilization(
        &self,
        quantity: &str,
        unit_symbol: &str,
        ai_multiplier: &str,
    ) -> Result<PhysicalQuantity, Status> {
        // 1. Resolve unit metadata from the clinical registry
        let entry = UnitRegistry::resolve_symbol(unit_symbol).map_err(|e| {
            self.invalid_argument(format!(
                "Physical Invariant Violation: Invalid unit '{}' ({}).",
                unit_symbol, e
            ))
        })?;

        // 2. Parse the AI resolved multiplier
        let ai_val = Decimal::from_str(ai_multiplier).map_err(|_| {
            self.internal_error(format!(
                "AI Hallucination: Malformed multiplier '{}' for contextual unit.",
                ai_multiplier
            ))
        })?;

        // 3. Safe Delegation: Determine Source of Authority
        let base_quantity_res = if entry.is_contextual {
            // Authority: AI (Contextual units like 'pack' or 'misc')
            UnitRegistry::parse_and_convert_with_multiplier(quantity, unit_symbol, ai_val)
        } else {
            // Authority: REGISTRY (Universal constants like 'kg' or 'l')
            // We ignore the AI's multiplier resolution to ensure physical laws are not
            // redefined.
            UnitRegistry::parse_and_convert(quantity, unit_symbol)
        };

        let base_quantity = base_quantity_res.map_err(|e| {
            self.invalid_argument(format!(
                "Physical Invariant Violation: Stabilization failed ({}).",
                e
            ))
        })?;

        // 4. Final Result Guard: Strictly Positive mass/volume/count
        if base_quantity.value().is_sign_negative() || base_quantity.value().is_zero() {
            return Err(self.invalid_argument(
                "Physical Invariant Violation: Stabilized quantity must be strictly positive.",
            ));
        }

        Ok(base_quantity)
    }

    /// Captures the un-normalized human intent for the audit log.
    fn format_raw_input(&self, intent: &MutationIntent) -> String {
        let op = match OperationType::try_from(intent.operation) {
            Ok(OperationType::Add) => "Add",
            Ok(OperationType::Subtract) => "Sub",
            Ok(OperationType::Set) => "Set",
            Ok(OperationType::Delete) => "Delete",
            _ => "Unknown",
        };

        format!(
            "{} {} {} {}",
            op,
            intent.quantity.as_deref().unwrap_or(""),
            intent.unit.as_deref().unwrap_or(""),
            intent.item_key
        )
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
    }

    /// Physical Invariant Check: Enforces the Dimensional Fence (ADR 008).
    fn enforce_physical_invariants(
        &self,
        intent: &MutationIntent,
        resolved_key: &str,
        new_quantity: &PhysicalQuantity,
        current_inventory: &[common::proto::v1::app::GroceryItem],
    ) -> Result<(), Status> {
        if intent.operation == OperationType::Add as i32
            || intent.operation == OperationType::Subtract as i32
        {
            if let Some(existing_item) = current_inventory
                .iter()
                .find(|i| i.item_key == resolved_key)
            {
                let existing_unit =
                    UnitRegistry::resolve_symbol(&existing_item.unit).map_err(|e| {
                        self.internal_error(format!(
                            "Internal state corruption: Existing item has invalid unit '{}' ({})",
                            existing_item.unit, e
                        ))
                    })?;

                if existing_unit.dimension != new_quantity.dimension() {
                    return Err(self.invalid_argument(format!(
                        "Physical Invariant Violation: Cannot perform arithmetic between {:?} and \
                         {:?} (Dimensional Fence).",
                        existing_unit.dimension,
                        new_quantity.dimension()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Phase 2 Implementation: Semantic validation via AI Veto Relay.
    async fn evaluate_policy(
        &self,
        client_id: String,
        intent: &MutationIntent,
    ) -> Result<VetoOutcome, Status> {
        info!("Triggering AI Veto evaluation for normalized intent...");
        let outcome = self
            .veto_relay
            .evaluate(
                client_id,
                intent,
                &[], // Inventory store implemented in Phase 5
                self.veto_timeout,
                self.max_justification_len,
            )
            .await;

        match outcome {
            Ok(v) => {
                if !v.is_approved {
                    info!("Mutation VETOED by AI: {}", v.moral_justification);
                }
                Ok(v)
            }
            Err(VetoError::Timeout(d)) => {
                warn!("AI Veto evaluation timed out after {:?}", d);
                Err(Status::deadline_exceeded(
                    "AI evaluation timed out. Please retry shortly.",
                ))
            }
            Err(e) => {
                error!("AI Veto infrastructure failure: {}", e);
                Err(self.internal_error("Internal policy engine failure"))
            }
        }
    }

    /// Phase 3 & 4 Implementation: Consensus and Quorum.
    async fn commit_to_consensus(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
        intent: MutationIntent,
        stabilized: StabilizedMutation,
        raw_user_input: String,
    ) -> Result<LogIndex, Status> {
        let is_delete = intent.operation == OperationType::Delete as i32;

        let mutation = CommittedMutation::new(
            client_id,
            sequence_id,
            stabilized.resolved_item_key,
            stabilized.suggested_display_name,
            stabilized.updated_base_quantity,
            stabilized.base_unit,
            stabilized.display_unit,
            stabilized.category.to_string(),
            raw_user_input,
            stabilized.moral_justification,
            is_delete,
            std::time::SystemTime::now(),
        );

        let mut command = Vec::new();
        mutation
            .encode(&mut command)
            .map_err(|e| self.internal_error(e.to_string()))?;

        let proposal_index = self.raft_handle.propose(command).await?;

        info!(
            "Mutation index {} appended. Waiting for quorum...",
            proposal_index
        );

        self.raft_handle.await_commit(proposal_index).await?;
        Ok(proposal_index)
    }
}

#[tonic::async_trait]
impl IngressService for IngressDispatcher {
    /// High-level orchestrator for user mutations.
    /// Implements the Defensive Onion pipeline (ADR 007).
    async fn propose_mutation(
        &self,
        request: Request<ProposeMutationRequest>,
    ) -> Result<Response<ProposeMutationResponse>, Status> {
        let req = request.into_inner();

        let sequence_id = SequenceId::new(req.sequence_id);
        let client_id = req
            .client_id
            .parse::<ClientId>()
            .map_err(|e| self.invalid_argument(format!("Invalid client_id: {}", e)))?;

        let span = info_span!("propose_mutation", client = %client_id, seq = %sequence_id);
        let _enter = span.enter();

        // --- Phase 0: Leadership Authority & Consensus Status ---
        let status = self.raft_handle.consensus_status().await;
        if !status.is_leader {
            return Ok(self.rejection_response_with_status(status));
        }

        // --- Phase 1: Deduplication (Layer 2 - EOS) ---
        if let Some(original_index) = self
            .raft_handle
            .check_session(&client_id, sequence_id)
            .await?
        {
            info!(
                "Duplicate request detected for client {} (seq {}). Returning cached index {}.",
                client_id, sequence_id, original_index
            );
            return Ok(Response::new(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                state_version: original_index.value(),
                leader_hint: String::new(),
                error_message: String::new(),
            }));
        }

        // --- Phase 2: Concurrency Control (Layer 2) ---
        let _lock = self.acquire_mutation_lock().await;

        // --- Phase 3 & 4: Semantic Resolution & Validation Loop ---
        let mut intent = req.intent.clone().ok_or_else(|| {
            self.invalid_argument("ProposeMutationRequest is missing 'intent' field")
        })?;
        let raw_user_input = self.format_raw_input(&intent);
        self.normalize_intent(&mut intent)?;

        let mut last_error_message = String::new();
        let mut stabilized_mutation = None;

        for attempt in 0..=self.veto_max_retries {
            if attempt > 0 {
                info!(
                    "Retrying AI resolution (attempt {}/{})...",
                    attempt + 1,
                    self.veto_max_retries + 1
                );
            }

            // --- Phase 3: Semantic AI Policy Egress (Layer 3) ---
            let veto = match self.evaluate_policy(req.client_id.clone(), &intent).await {
                Ok(v) => v,
                Err(e) if attempt < self.veto_max_retries => {
                    warn!(
                        "Transient AI failure on attempt {}: {}. Retrying...",
                        attempt + 1,
                        e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };

            if !veto.is_approved {
                return Ok(Response::new(ProposeMutationResponse {
                    status: MutationStatus::Vetoed as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: veto.moral_justification,
                }));
            }

            // --- Phase 4: Validation Proxy & Physical Invariants (Layer 4) ---
            match self.validate_and_stabilize(&intent, &veto, &[]) {
                Ok(s) => {
                    stabilized_mutation = Some(s);
                    break;
                }
                Err(status) if attempt < self.veto_max_retries => {
                    warn!(
                        "AI response failed Layer 4 validation on attempt {}: {}. Retrying...",
                        attempt + 1,
                        status.message()
                    );
                    last_error_message = status.message().to_string();
                    continue;
                }
                Err(status) => {
                    warn!(
                        "AI resolution exhausted retries and failed Layer 4 validation: {}",
                        status.message()
                    );
                    return Ok(Response::new(ProposeMutationResponse {
                        status: MutationStatus::Vetoed as i32,
                        state_version: 0,
                        leader_hint: String::new(),
                        error_message: status.message().to_string(),
                    }));
                }
            }
        }

        let stabilized = stabilized_mutation.ok_or_else(|| {
            self.internal_error(format!(
                "Retry loop exhausted without result: {}",
                last_error_message
            ))
        })?;

        // --- Phase 5: Consensus Proposal & Quorum (Layer 5) ---
        let proposal_index = self
            .commit_to_consensus(&client_id, sequence_id, intent, stabilized, raw_user_input)
            .await?;

        info!("Mutation index {} committed successfully.", proposal_index);
        Ok(Response::new(ProposeMutationResponse {
            status: MutationStatus::Committed as i32,
            state_version: proposal_index.value(),
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }

    /// High-level orchestrator for state queries.
    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let req = request.into_inner();

        let span = info_span!("query_state");
        let _enter = span.enter();

        // 1. Leadership Authority (Quorum Read Verification)
        if let Err(status) = self.raft_handle.verify_leadership().await {
            let consensus_status = self.raft_handle.consensus_status().await;
            return Ok(Response::new(QueryStateResponse {
                items: Vec::new(),
                current_state_version: 0,
                status: QueryStatus::Rejected as i32,
                leader_hint: consensus_status.leader_hint,
                error_message: status.message().to_string(),
            }));
        }

        // 2. Fetch inventory from the authoritative state machine
        let all_items = self.inventory_source.get_inventory().await;

        // 3. Apply semantic filters
        let filtered_items = if let Some(filter) = req.query_filter {
            let filter = filter.to_lowercase();
            all_items
                .into_iter()
                .filter(|item| item.item_key.to_lowercase().contains(&filter))
                .collect()
        } else {
            all_items
        };

        // TODO: Step 4 - Wait for min_state_version (EOS Barrier)

        Ok(Response::new(QueryStateResponse {
            items: filtered_items,
            current_state_version: 0, // TODO: Return actual index from Store
            status: QueryStatus::Success as i32,
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    #[derive(Debug, Default)]
    struct MockRaftHandle {
        is_leader: bool,
        leader_hint: String,
        rejection_reason: String,
        proposals: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl RaftHandle for MockRaftHandle {
        async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, Status> {
            self.proposals.lock().unwrap().push(data);
            Ok(LogIndex::new(1))
        }

        async fn await_commit(&self, _index: LogIndex) -> Result<(), Status> {
            Ok(())
        }

        async fn consensus_status(&self) -> ConsensusStatus {
            ConsensusStatus {
                is_leader: self.is_leader,
                leader_hint: self.leader_hint.clone(),
                rejection_reason: self.rejection_reason.clone(),
            }
        }

        async fn check_session(
            &self,
            _client_id: &ClientId,
            _sequence_id: SequenceId,
        ) -> Result<Option<LogIndex>, Status> {
            Ok(None)
        }

        async fn verify_leadership(&self) -> Result<(), Status> {
            if self.is_leader {
                Ok(())
            } else {
                Err(Status::failed_precondition("Not leader"))
            }
        }
    }

    #[derive(Debug, Default)]
    struct MockVetoRelay {
        outcome: Option<VetoOutcome>,
        error: Option<VetoError>,
    }

    #[async_trait]
    impl VetoRelay for MockVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &MutationIntent,
            _current_inventory: &[common::proto::v1::app::GroceryItem],
            _timeout: Duration,
            _max_justification_len: usize,
        ) -> Result<VetoOutcome, VetoError> {
            if let Some(err) = &self.error {
                return Err(err.clone());
            }
            Ok(self.outcome.clone().unwrap_or_else(valid_outcome))
        }
    }

    #[derive(Debug, Default)]
    struct FlakyVetoRelay {
        outcome: Option<VetoOutcome>,
        fail_count: Mutex<usize>,
        max_fails: usize,
    }

    #[async_trait]
    impl VetoRelay for FlakyVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &MutationIntent,
            _current_inventory: &[common::proto::v1::app::GroceryItem],
            _timeout: Duration,
            _max_justification_len: usize,
        ) -> Result<VetoOutcome, VetoError> {
            let mut count = self.fail_count.lock().unwrap();
            if *count < self.max_fails {
                *count += 1;
                return Err(VetoError::Timeout(Duration::from_secs(0)));
            }
            Ok(self.outcome.clone().unwrap_or_else(|| {
                let mut v = valid_outcome();
                v.moral_justification = "Recovered".to_string();
                v
            }))
        }
    }

    #[derive(Debug)]
    struct HallucinatingVetoRelay {
        success_outcome: VetoOutcome,
        hallucination_outcome: VetoOutcome,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl VetoRelay for HallucinatingVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &MutationIntent,
            _current_inventory: &[common::proto::v1::app::GroceryItem],
            _timeout: Duration,
            _max_justification_len: usize,
        ) -> Result<VetoOutcome, VetoError> {
            let mut count = self.call_count.lock().unwrap();
            if *count == 0 {
                *count += 1;
                return Ok(self.hallucination_outcome.clone());
            }
            Ok(self.success_outcome.clone())
        }
    }

    #[derive(Debug)]
    struct MixedFailureVetoRelay {
        hallucination_outcome: VetoOutcome,
        call_count: Mutex<usize>,
    }

    #[async_trait]
    impl VetoRelay for MixedFailureVetoRelay {
        async fn evaluate(
            &self,
            _client_id: String,
            _intent: &MutationIntent,
            _current_inventory: &[common::proto::v1::app::GroceryItem],
            _timeout: Duration,
            _max_justification_len: usize,
        ) -> Result<VetoOutcome, VetoError> {
            let mut count = self.call_count.lock().unwrap();
            let current = *count;
            *count += 1;

            match current {
                0 => Err(VetoError::Timeout(Duration::from_secs(0))),
                _ => Ok(self.hallucination_outcome.clone()),
            }
        }
    }

    fn valid_outcome() -> VetoOutcome {
        VetoOutcome {
            is_approved: true,
            category_assignment: "Primary Flora".to_string(),
            moral_justification: "Mock justification".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "ml".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        }
    }

    #[derive(Debug, Default)]
    struct MockInventorySource {
        items: Vec<common::proto::v1::app::GroceryItem>,
    }

    #[async_trait]
    impl InventorySource for MockInventorySource {
        async fn get_inventory(&self) -> Vec<common::proto::v1::app::GroceryItem> {
            self.items.clone()
        }
    }

    fn mock_dispatcher(
        raft_handle: Arc<dyn RaftHandle>,
        veto_relay: Arc<dyn VetoRelay>,
    ) -> IngressDispatcher {
        IngressDispatcher::new(
            raft_handle,
            Arc::new(MockInventorySource::default()),
            veto_relay,
            Duration::from_secs(1),
            1,
            512,
        )
    }

    mod propose_mutation {
        use super::*;

        fn successful_raft() -> Arc<MockRaftHandle> {
            Arc::new(MockRaftHandle {
                is_leader: true,
                ..Default::default()
            })
        }

        fn successful_veto() -> Arc<MockVetoRelay> {
            Arc::new(MockVetoRelay {
                outcome: Some(valid_outcome()),
                ..Default::default()
            })
        }

        // --- Phase 0: Leadership Authority ---

        #[tokio::test]
        async fn returns_rejected_when_not_leader() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: false,
                leader_hint: "http://leader:50051".to_string(),
                rejection_reason: "Node is a Follower".to_string(),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: None,
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert_eq!(response.leader_hint, "http://leader:50051");
            assert!(response.error_message.contains("Follower"));
        }

        // --- Phase 1: Deduplication (EOS) ---

        #[tokio::test]
        async fn returns_cached_success_on_duplicate_sequence() {
            #[derive(Debug)]
            struct DuplicateRaft {
                mock: Arc<MockRaftHandle>,
                committed_index: LogIndex,
            }
            #[async_trait]
            impl RaftHandle for DuplicateRaft {
                async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, Status> {
                    self.mock.propose(data).await
                }

                async fn await_commit(&self, index: LogIndex) -> Result<(), Status> {
                    self.mock.await_commit(index).await
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    self.mock.consensus_status().await
                }

                async fn check_session(
                    &self,
                    _client_id: &ClientId,
                    _sequence_id: SequenceId,
                ) -> Result<Option<LogIndex>, Status> {
                    Ok(Some(self.committed_index))
                }

                async fn verify_leadership(&self) -> Result<(), Status> {
                    self.mock.verify_leadership().await
                }
            }

            let committed_index = LogIndex::new(42);
            let raft_with_dup = Arc::new(DuplicateRaft {
                mock: successful_raft(),
                committed_index,
            });

            let dispatcher = mock_dispatcher(raft_with_dup, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

            assert_eq!(response.status, MutationStatus::Committed as i32);
            assert_eq!(response.state_version, committed_index.value());
        }

        // --- Phase 2: Concurrency & Syntactic (Layer 2) ---

        #[tokio::test]
        async fn normalizes_intent_syntactically() {
            let raft = successful_raft();
            let dispatcher = mock_dispatcher(
                raft.clone(),
                Arc::new(MockVetoRelay {
                    outcome: Some(VetoOutcome {
                        is_approved: true,
                        category_assignment: "Primary Flora".to_string(),
                        moral_justification: "Mock justification".to_string(),
                        resolved_item_key: "bananas".to_string(),
                        suggested_display_name: "Bananas".to_string(),
                        resolved_unit: "units".to_string(),
                        conversion_multiplier_to_base: "1".to_string(),
                    }),
                    ..Default::default()
                }),
            );
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "  BANANAS  ".to_string(),
                    quantity: Some(" 5 ".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let _ = dispatcher.propose_mutation(req).await.unwrap();

            let proposals = raft.proposals.lock().unwrap();
            assert_eq!(proposals.len(), 1);
            let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
            assert_eq!(mutation.resolved_item_key, "bananas");
            assert_eq!(mutation.updated_base_quantity, "5");
        }

        #[tokio::test]
        async fn verifies_full_consensus_serialization() {
            let raft = successful_raft();
            let dispatcher = mock_dispatcher(
                raft.clone(),
                Arc::new(MockVetoRelay {
                    outcome: Some(VetoOutcome {
                        is_approved: true,
                        category_assignment: "Animal Secretions".to_string(),
                        moral_justification: "Milk is ethical".to_string(),
                        resolved_item_key: "milk-whole".to_string(),
                        suggested_display_name: "Whole Milk".to_string(),
                        resolved_unit: "gal".to_string(),
                        conversion_multiplier_to_base: "3785.4118".to_string(),
                    }),
                    ..Default::default()
                }),
            );
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 42,
                intent: Some(MutationIntent {
                    item_key: "  MiLk  ".to_string(),
                    quantity: Some(" 1.5 ".to_string()),
                    unit: Some(" gal ".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

            assert_eq!(response.status, MutationStatus::Committed as i32);

            let proposals = raft.proposals.lock().unwrap();
            assert_eq!(proposals.len(), 1);
            let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();

            // Verification of SI Stabilization (1.5 * 3785.4118 = 5678.1177)
            assert_eq!(mutation.resolved_item_key, "milk-whole");
            assert_eq!(mutation.updated_base_quantity, "5678.1177");
            assert_eq!(mutation.base_unit, "ml");
            assert_eq!(mutation.display_unit, "gal");

            // Verification of RAW Audit Log (Must preserve original messy input)
            assert_eq!(mutation.raw_user_input, "Add 1.5 gal MiLk");
        }

        #[tokio::test]
        async fn rejects_missing_quantity_for_add_operation() {
            let dispatcher = mock_dispatcher(successful_raft(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: None,
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }

        #[tokio::test]
        async fn successfully_handles_delete_operation() {
            let raft = successful_raft();
            let dispatcher = mock_dispatcher(
                raft.clone(),
                Arc::new(MockVetoRelay {
                    outcome: Some(VetoOutcome {
                        is_approved: true,
                        category_assignment: "Animal Secretions".to_string(),
                        moral_justification: "Item removed".to_string(),
                        resolved_item_key: "milk-whole".to_string(),
                        suggested_display_name: "Whole Milk".to_string(),
                        resolved_unit: "ml".to_string(),
                        conversion_multiplier_to_base: "1".to_string(),
                    }),
                    ..Default::default()
                }),
            );
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 42,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: None,
                    operation: OperationType::Delete as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

            assert_eq!(response.status, MutationStatus::Committed as i32);

            let proposals = raft.proposals.lock().unwrap();
            let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
            assert!(mutation.is_delete);
            assert_eq!(mutation.resolved_item_key, "milk-whole");
        }

        #[tokio::test]
        async fn rejects_when_item_key_is_empty() {
            let dispatcher = mock_dispatcher(successful_raft(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "   ".to_string(),
                    quantity: Some("5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }

        #[tokio::test]
        async fn rejects_when_category_hint_is_invalid() {
            let dispatcher = mock_dispatcher(successful_raft(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("5".to_string()),
                    category: Some("Forbidden Snacks".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains("Invalid category hint"));
        }

        #[tokio::test]
        async fn rejects_negative_quantity_for_mutation_intents() {
            let dispatcher = mock_dispatcher(successful_raft(), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("-5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains("cannot be negative"));
        }

        #[tokio::test]
        async fn enforces_sequential_processing_via_lock() {
            use std::sync::atomic::AtomicUsize;
            use std::sync::atomic::Ordering;

            use tokio::time::Duration;
            use tokio::time::sleep;

            #[derive(Debug)]
            struct SlowVetoRelay {
                active_calls: Arc<AtomicUsize>,
                max_concurrent: Arc<AtomicUsize>,
            }

            #[async_trait]
            impl VetoRelay for SlowVetoRelay {
                async fn evaluate(
                    &self,
                    _client_id: String,
                    _intent: &MutationIntent,
                    _current_inventory: &[common::proto::v1::app::GroceryItem],
                    _timeout: Duration,
                    _max_justification_len: usize,
                ) -> Result<VetoOutcome, VetoError> {
                    let current = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;

                    let mut max = self.max_concurrent.load(Ordering::SeqCst);
                    while current > max {
                        match self.max_concurrent.compare_exchange_weak(
                            max,
                            current,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => break,
                            Err(actual) => max = actual,
                        }
                    }

                    sleep(Duration::from_millis(100)).await;
                    self.active_calls.fetch_sub(1, Ordering::SeqCst);

                    Ok(valid_outcome())
                }
            }

            let raft = successful_raft();
            let active_calls = Arc::new(AtomicUsize::new(0));
            let max_concurrent = Arc::new(AtomicUsize::new(0));

            let veto = Arc::new(SlowVetoRelay {
                active_calls: active_calls.clone(),
                max_concurrent: max_concurrent.clone(),
            });

            let dispatcher = Arc::new(mock_dispatcher(raft, veto));

            let d1 = dispatcher.clone();
            let h1 = tokio::spawn(async move {
                let req = Request::new(ProposeMutationRequest {
                    client_id: ClientId::generate().as_str().to_string(),
                    sequence_id: 1,
                    intent: Some(MutationIntent {
                        item_key: "item1".to_string(),
                        quantity: Some("1".to_string()),
                        operation: OperationType::Add as i32,
                        ..Default::default()
                    }),
                });
                d1.propose_mutation(req).await
            });

            let d2 = dispatcher.clone();
            let h2 = tokio::spawn(async move {
                let req = Request::new(ProposeMutationRequest {
                    client_id: ClientId::generate().as_str().to_string(),
                    sequence_id: 2,
                    intent: Some(MutationIntent {
                        item_key: "item2".to_string(),
                        quantity: Some("2".to_string()),
                        operation: OperationType::Add as i32,
                        ..Default::default()
                    }),
                });
                d2.propose_mutation(req).await
            });

            let _ = tokio::try_join!(h1, h2).unwrap();

            assert_eq!(
                max_concurrent.load(Ordering::SeqCst),
                1,
                "Mutations were processed concurrently!"
            );
        }

        // --- Phase 3: Semantic AI Resolution (Layer 3) ---

        #[tokio::test]
        async fn returns_vetoed_when_ai_rejects() {
            let veto = Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: false,
                    category_assignment: "Primary Flora".to_string(),
                    moral_justification: "Mock justification".to_string(),
                    resolved_item_key: "milk".to_string(),
                    suggested_display_name: "Milk".to_string(),
                    resolved_unit: "ml".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(successful_raft(), veto);
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(response.error_message.contains("Mock justification"));
        }

        #[tokio::test]
        async fn returns_error_on_ai_timeout() {
            let veto = Arc::new(MockVetoRelay {
                error: Some(VetoError::Timeout(Duration::from_secs(1))),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(successful_raft(), veto);
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        }

        // --- Phase 4 & 5: Consensus & State Machine (Layer 4/5) ---

        #[tokio::test]
        async fn returns_vetoed_when_ai_hallucinates_metadata() {
            let dispatcher = mock_dispatcher(
                successful_raft(),
                Arc::new(MockVetoRelay {
                    outcome: Some(VetoOutcome {
                        is_approved: true,
                        category_assignment: "Space Matter".to_string(), // Hallucination
                        moral_justification: "Approved".to_string(),
                        resolved_item_key: "milk".to_string(),
                        suggested_display_name: "Milk".to_string(),
                        resolved_unit: "g".to_string(),
                        conversion_multiplier_to_base: "1".to_string(),
                    }),
                    ..Default::default()
                }),
            );
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(response.error_message.contains("AI Hallucination"));
        }

        #[tokio::test]
        async fn returns_vetoed_when_ai_provides_invalid_conversion() {
            let dispatcher = mock_dispatcher(
                successful_raft(),
                Arc::new(MockVetoRelay {
                    outcome: Some(VetoOutcome {
                        is_approved: true,
                        category_assignment: "Primary Flora".to_string(),
                        moral_justification: "Approved".to_string(),
                        resolved_item_key: "milk".to_string(),
                        suggested_display_name: "Milk".to_string(),
                        resolved_unit: "blorgs".to_string(), // Hallucination
                        conversion_multiplier_to_base: "1".to_string(),
                    }),
                    ..Default::default()
                }),
            );
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(
                response
                    .error_message
                    .contains("Physical Invariant Violation")
            );
        }

        #[tokio::test]
        async fn returns_error_on_consensus_failure() {
            #[derive(Debug, Default)]
            struct FailingRaft;
            #[async_trait]
            impl RaftHandle for FailingRaft {
                async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, Status> {
                    Err(Status::internal("Consensus failure"))
                }

                async fn await_commit(&self, _index: LogIndex) -> Result<(), Status> {
                    Ok(())
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    ConsensusStatus {
                        is_leader: true,
                        ..Default::default()
                    }
                }

                async fn check_session(
                    &self,
                    _client_id: &ClientId,
                    _sequence_id: SequenceId,
                ) -> Result<Option<LogIndex>, Status> {
                    Ok(None)
                }

                async fn verify_leadership(&self) -> Result<(), Status> {
                    Ok(())
                }
            }

            let dispatcher = mock_dispatcher(Arc::new(FailingRaft), successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "bananas".to_string(),
                    quantity: Some("5".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let result = dispatcher.propose_mutation(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }

        #[tokio::test]
        async fn retries_on_transient_ai_failure_and_succeeds() {
            let raft = successful_raft();
            let veto = Arc::new(FlakyVetoRelay {
                max_fails: 1,
                ..Default::default()
            });
            // Configured for 1 retry (max 2 attempts)
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                Arc::new(MockInventorySource::default()),
                veto,
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Committed as i32);
        }

        #[tokio::test]
        async fn retries_on_ai_hallucination_and_succeeds() {
            let raft = successful_raft();
            let hallucination = VetoOutcome {
                is_approved: true,
                category_assignment: "Space Matter".to_string(), // Hallucination
                moral_justification: "Oops".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };
            let success = VetoOutcome {
                is_approved: true,
                category_assignment: "Animal Secretions".to_string(),
                moral_justification: "Corrected".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };
            let veto = Arc::new(HallucinatingVetoRelay {
                hallucination_outcome: hallucination,
                success_outcome: success,
                call_count: Mutex::new(0),
            });
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                Arc::new(MockInventorySource::default()),
                veto,
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Committed as i32);
        }

        #[tokio::test]
        async fn vetoes_after_max_retries_exhausted_on_hallucination() {
            let raft = successful_raft();
            let hallucination = VetoOutcome {
                is_approved: true,
                category_assignment: "Space Matter".to_string(),
                moral_justification: "Still Hallucinating".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };
            let veto = Arc::new(MockVetoRelay {
                outcome: Some(hallucination),
                ..Default::default()
            });
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                Arc::new(MockInventorySource::default()),
                veto,
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(response.error_message.contains("AI Hallucination"));
        }

        #[tokio::test]
        async fn vetoes_after_max_retries_exhausted_on_mixed_failures() {
            let raft = successful_raft();
            let hallucination = VetoOutcome {
                is_approved: true,
                category_assignment: "Space Matter".to_string(), // Hallucination
                moral_justification: "Oops".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };
            let veto = Arc::new(MixedFailureVetoRelay {
                hallucination_outcome: hallucination,
                call_count: Mutex::new(0),
            });
            // Configured for 1 retry (2 attempts total)
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                Arc::new(MockInventorySource::default()),
                veto,
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

            // Should be VETOED because:
            // Attempt 1: Timeout (Consumed 1st attempt)
            // Attempt 2: Hallucination (Consumed 1 retry quota)
            // Quota exhausted -> definitive Veto
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert!(response.error_message.contains("AI Hallucination"));
        }

        #[tokio::test]
        async fn does_not_retry_on_definitive_ai_veto() {
            use std::sync::atomic::AtomicUsize;
            use std::sync::atomic::Ordering;

            #[derive(Debug)]
            struct CountingVetoRelay {
                call_count: Arc<AtomicUsize>,
            }
            #[async_trait]
            impl VetoRelay for CountingVetoRelay {
                async fn evaluate(
                    &self,
                    _client_id: String,
                    _intent: &MutationIntent,
                    _current_inventory: &[common::proto::v1::app::GroceryItem],
                    _timeout: Duration,
                    _max_justification_len: usize,
                ) -> Result<VetoOutcome, VetoError> {
                    self.call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(VetoOutcome {
                        is_approved: false,
                        moral_justification: "Definitive NO".to_string(),
                        ..valid_outcome()
                    })
                }
            }

            let raft = successful_raft();
            let call_count = Arc::new(AtomicUsize::new(0));
            let veto = Arc::new(CountingVetoRelay {
                call_count: call_count.clone(),
            });
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                Arc::new(MockInventorySource::default()),
                veto,
                Duration::from_secs(1),
                10,
                512,
            );

            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1,
                intent: Some(MutationIntent {
                    item_key: "unethical item".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let _ = dispatcher.propose_mutation(req).await.unwrap();
            assert_eq!(call_count.load(Ordering::SeqCst), 1);
        }
    }

    mod validate_and_stabilize {
        use common::proto::v1::app::GroceryItem;

        use super::*;

        fn test_dispatcher() -> IngressDispatcher {
            mock_dispatcher(
                Arc::new(MockRaftHandle::default()),
                Arc::new(MockVetoRelay::default()),
            )
        }

        #[test]
        fn rejects_hallucinated_category() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1".to_string()),
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                category_assignment: "Space Matter".to_string(), // Hallucination
                moral_justification: "Approved".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "g".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };

            let result = dispatcher.validate_and_stabilize(&intent, &veto, &[]);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }

        #[test]
        fn rejects_hallucinated_unit() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1".to_string()),
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Approved".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "blorgs".to_string(), // Hallucination
                conversion_multiplier_to_base: "1".to_string(),
            };

            let result = dispatcher.validate_and_stabilize(&intent, &veto, &[]);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }

        #[test]
        fn rejects_invalid_si_unit_conversion() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("abc".to_string()), // Malformed quantity
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Approved".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "g".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };

            let result = dispatcher.validate_and_stabilize(&intent, &veto, &[]);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }

        #[test]
        fn rejects_cross_dimensional_arithmetic() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1".to_string()),
                operation: OperationType::Add as i32,
                ..Default::default()
            };
            // AI resolves a liquid unit for an item that exists as weight
            let veto = VetoOutcome {
                is_approved: true,
                resolved_item_key: "milk".to_string(),
                category_assignment: "Animal Secretions".to_string(),
                moral_justification: "Approved".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            };
            let inventory = vec![GroceryItem {
                item_key: "milk".to_string(),
                unit: "g".to_string(), // Dimension: Mass
                ..Default::default()
            }];

            let result = dispatcher.validate_and_stabilize(&intent, &veto, &inventory);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("Dimensional Fence"));
        }

        #[test]
        fn applies_bankers_rounding_to_si_stabilization() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1.5".to_string()), // Half-way point
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Approved".to_string(),
                resolved_item_key: "item".to_string(),
                suggested_display_name: "Item".to_string(),
                resolved_unit: "lb".to_string(), // 1 lb = 453.59237 g
                conversion_multiplier_to_base: "453.59237".to_string(),
            };

            let result = dispatcher
                .validate_and_stabilize(&intent, &veto, &[])
                .unwrap();

            // 1.5 * 453.59237 = 680.388555
            // Banker's Rounding to 4 dp as defined in units.rs
            assert_eq!(result.updated_base_quantity, "680.3886");
            assert_eq!(result.base_unit, "g");
        }

        #[test]
        fn grants_contextual_override_when_unit_is_dynamic() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("2".to_string()),
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                resolved_unit: "pack".to_string(), // Contextual unit
                conversion_multiplier_to_base: "6".to_string(), // AI resolves 6 per pack
                ..valid_outcome()
            };

            let result = dispatcher
                .validate_and_stabilize(&intent, &veto, &[])
                .unwrap();
            // 2 packs * 6 multiplier = 12 base units
            assert_eq!(result.updated_base_quantity, "12");
        }

        #[test]
        fn ignores_physical_constant_redefinition_when_unit_is_static() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1".to_string()),
                ..Default::default()
            };
            let veto = VetoOutcome {
                is_approved: true,
                resolved_unit: "kg".to_string(), // Static unit
                conversion_multiplier_to_base: "500".to_string(), /* AI attempts to redefine 1kg
                                                  * = 500g */
                ..valid_outcome()
            };

            let result = dispatcher
                .validate_and_stabilize(&intent, &veto, &[])
                .unwrap();

            // Physical Law Check: Registry (1000) must override AI (500)
            assert_eq!(result.updated_base_quantity, "1000");
            assert_eq!(result.base_unit, "g");
        }

        #[test]
        fn rejects_non_positive_quantity_during_stabilization() {
            let dispatcher = test_dispatcher();
            let intent = MutationIntent {
                quantity: Some("1".to_string()),
                ..Default::default()
            };

            // Test 1: Zero (using contextual unit to ensure AI multiplier is applied)
            let veto_zero = VetoOutcome {
                is_approved: true,
                resolved_unit: "pack".to_string(),
                conversion_multiplier_to_base: "0".to_string(),
                ..valid_outcome()
            };
            let status_zero = dispatcher
                .validate_and_stabilize(&intent, &veto_zero, &[])
                .unwrap_err();
            assert_eq!(status_zero.code(), tonic::Code::InvalidArgument);

            // Test 2: Negative
            let veto_neg = VetoOutcome {
                is_approved: true,
                resolved_unit: "pack".to_string(),
                conversion_multiplier_to_base: "-1".to_string(),
                ..valid_outcome()
            };
            let status_neg = dispatcher
                .validate_and_stabilize(&intent, &veto_neg, &[])
                .unwrap_err();
            assert_eq!(status_neg.code(), tonic::Code::InvalidArgument);
            assert!(status_neg.message().contains("strictly positive"));
        }
    }

    mod query_state {
        use super::*;

        #[tokio::test]
        async fn returns_rejected_when_not_leader() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: false,
                leader_hint: "http://leader:50051".to_string(),
                rejection_reason: "Node is a Follower".to_string(),
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, Arc::new(MockVetoRelay::default()));
            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Rejected as i32);
            assert_eq!(response.leader_hint, "http://leader:50051");
        }

        #[tokio::test]
        async fn returns_all_items_when_no_filter_is_provided() {
            let items = vec![
                GroceryItem {
                    item_key: "milk".to_string(),
                    quantity: "1000".to_string(),
                    unit: "ml".to_string(),
                    ..Default::default()
                },
                GroceryItem {
                    item_key: "eggs".to_string(),
                    quantity: "12".to_string(),
                    unit: "units".to_string(),
                    ..Default::default()
                },
            ];

            let raft = Arc::new(MockRaftHandle {
                is_leader: true,
                ..Default::default()
            });
            let inventory = Arc::new(MockInventorySource {
                items: items.clone(),
            });
            let dispatcher = IngressDispatcher::new(
                raft,
                inventory,
                Arc::new(MockVetoRelay::default()),
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Success as i32);
            assert_eq!(response.items.len(), 2);
            assert_eq!(response.items[0].item_key, "milk");
            assert_eq!(response.items[1].item_key, "eggs");
        }

        #[tokio::test]
        async fn filters_items_by_substring_match() {
            let items = vec![
                GroceryItem {
                    item_key: "milk-whole".to_string(),
                    ..Default::default()
                },
                GroceryItem {
                    item_key: "milk-skim".to_string(),
                    ..Default::default()
                },
                GroceryItem {
                    item_key: "eggs".to_string(),
                    ..Default::default()
                },
            ];

            let raft = Arc::new(MockRaftHandle {
                is_leader: true,
                ..Default::default()
            });
            let inventory = Arc::new(MockInventorySource {
                items: items.clone(),
            });
            let dispatcher = IngressDispatcher::new(
                raft,
                inventory,
                Arc::new(MockVetoRelay::default()),
                Duration::from_secs(1),
                1,
                512,
            );

            let req = Request::new(QueryStateRequest {
                query_filter: Some("milk".to_string()),
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Success as i32);
            assert_eq!(response.items.len(), 2);
            assert!(response.items.iter().all(|i| i.item_key.contains("milk")));
        }
    }
}
