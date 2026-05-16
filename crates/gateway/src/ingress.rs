use std::fmt::Debug;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::OperationType;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_server::IngressService;
use common::raft_api::ConsensusHandle;
use common::raft_api::ConsensusStatus;
use common::raft_api::InventoryReader;
use common::raft_api::SessionProvider;
use common::taxonomy::GroceryCategory;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::types::errors::ConsensusError;
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
    raft_handle: Arc<dyn ConsensusHandle>,
    session_provider: Arc<dyn SessionProvider>,
    inventory_reader: Arc<dyn InventoryReader>,
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

        // 1. Verifies that this node is the authorized leader.
        let status = self.raft_handle.consensus_status().await;
        if !status.is_leader {
            return Ok(self.rejection_response_with_status(status));
        }

        // 2. Enforces linearizability via the sequence firewall.
        if let Some(cached_response) = self
            .enforce_sequence_firewall(&client_id, sequence_id)
            .await?
        {
            return Ok(cached_response);
        }

        // 3. Guards against concurrent mutation attempts.
        let _lock = self.mutation_lock.lock().await;

        // 4. Scrubs user input and ensures syntactic/taxonomic integrity.
        let mut intent = req.intent.clone().ok_or_else(|| {
            self.invalid_argument("ProposeMutationRequest is missing 'intent' field")
        })?;
        let raw_user_input = self.format_raw_input(&intent);
        self.normalize_intent(&mut intent)?;

        // 5. Resolves semantic metadata and stabilizes physical quantities via the AI
        //    resolution loop.
        let (final_status, stabilized) = self
            .resolve_semantic_mutation(req.client_id.clone(), &intent)
            .await?;

        // 6. Proposes the finalized intent to the cluster for consensus.
        let proposal_index = self
            .commit_to_consensus(
                &client_id,
                sequence_id,
                intent,
                stabilized.clone(),
                raw_user_input,
                final_status,
                &status,
            )
            .await?;

        info!(
            "Mutation index {} committed with status {:?}.",
            proposal_index, final_status
        );

        // 7. Constructs the final response reflecting the committed lifecycle status.
        Ok(self.build_mutation_response(
            proposal_index,
            final_status,
            stabilized.moral_justification,
        ))
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
        if let Err(err) = self.raft_handle.verify_leadership().await {
            let status = self.raft_handle.consensus_status().await;
            let grpc_status = self.map_consensus_error(err, &status);
            return Ok(Response::new(QueryStateResponse {
                items: Vec::new(),
                current_state_version: 0,
                status: QueryStatus::Rejected as i32,
                leader_hint: status.leader_hint,
                error_message: grpc_status.message().to_string(),
            }));
        }

        // 2. Consistent Horizon Check (Strict EOS)
        // Ensure the client isn't querying for a version that hasn't reached
        // cluster-wide agreement yet (ADR 006).
        let status = self.raft_handle.consensus_status().await;
        if let Some(min_version) = req.min_state_version.filter(|&v| v > 0) {
            let requested_index = LogIndex::new(min_version);

            if requested_index > status.commit_index {
                return Err(Status::failed_precondition(
                    "Requested version exceeds consistent horizon",
                ));
            }

            // 3. State Machine Convergence (Wait for apply)
            if let Err(err) = self.raft_handle.await_apply(requested_index).await {
                return Err(self.map_consensus_error(err, &status));
            }
        }

        // 4. Fetch inventory from the authoritative state machine
        let all_items = self.inventory_reader.get_inventory().await;
        let state_version = self.inventory_reader.current_version().await;

        // 5. Apply semantic filters
        let filtered_items = if let Some(filter) = req.query_filter {
            let filter = filter.to_lowercase();
            all_items
                .into_iter()
                .filter(|item| item.item_key.to_lowercase().contains(&filter))
                .collect()
        } else {
            all_items
        };

        Ok(Response::new(QueryStateResponse {
            items: filtered_items,
            current_state_version: state_version.value(),
            status: QueryStatus::Success as i32,
            leader_hint: String::new(),
            error_message: String::new(),
        }))
    }
}

impl IngressDispatcher {
    /// Creates a new IngressDispatcher with configured AI policy parameters.
    pub fn new(
        raft_handle: Arc<dyn ConsensusHandle>,
        session_provider: Arc<dyn SessionProvider>,
        inventory_reader: Arc<dyn InventoryReader>,
        veto_relay: Arc<dyn VetoRelay>,
        veto_timeout: Duration,
        veto_max_retries: usize,
        max_justification_len: usize,
    ) -> Self {
        Self {
            raft_handle,
            session_provider,
            inventory_reader,
            veto_relay,
            veto_timeout,
            veto_max_retries,
            max_justification_len,
            mutation_lock: Mutex::new(()),
        }
    }

    // --- High-Level Orchestration Helpers (Top-Down Call Order) ---

    /// Enforces Exactly-Once Semantics by validating request sequences against
    /// the authoritative session table.
    ///
    /// Returns a cached response if the request is a duplicate, or a logical
    /// rejection if it violates session continuity.
    async fn enforce_sequence_firewall(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
    ) -> Result<Option<Response<ProposeMutationResponse>>, Status> {
        if sequence_id.value() == 0 {
            return Ok(Some(Response::new(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                state_version: 0,
                leader_hint: String::new(),
                error_message: "Secure Clinical: Protocol Violation (Sequence ID 0)".to_string(),
            })));
        }

        let last_session = self
            .session_provider
            .check_session(client_id, SequenceId::new(0))
            .await
            .map_err(|e| self.invalid_argument(format!("Session lookup failed: {}", e)))?;

        if let Some(record) = last_session {
            if sequence_id.value() == record.last_sequence_id {
                info!(
                    "Deduplicating request for client {} (seq {}). Returning cached outcome.",
                    client_id, sequence_id
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

            if sequence_id.value() < record.last_sequence_id {
                warn!(
                    "Rejecting stale request for client {} (got {}, cluster at {}).",
                    client_id, sequence_id, record.last_sequence_id
                );
                return Ok(Some(Response::new(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    state_version: record.log_index,
                    leader_hint: String::new(),
                    error_message: "Secure Clinical: Stale Sequence".to_string(),
                })));
            }

            if sequence_id.value() > record.last_sequence_id + 1 {
                warn!(
                    "Rejecting sequence gap for client {}: expected {}, got {}.",
                    client_id,
                    record.last_sequence_id + 1,
                    sequence_id
                );
                return Ok(Some(Response::new(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    state_version: 0,
                    leader_hint: String::new(),
                    error_message: "Secure Clinical: Sequence Continuity Violation".to_string(),
                })));
            }
        } else if sequence_id.value() != 1 {
            // New client must start with sequence 1
            warn!(
                "Rejecting session bootstrap gap for client {}: expected 1, got {}.",
                client_id, sequence_id
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

    /// Normalizes user intents and enforces clinical taxonomy constraints
    /// before semantic resolution.
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

        if intent.operation != OperationType::Delete as i32 && intent.quantity.is_none() {
            return Err(self.invalid_argument("quantity is required for this operation"));
        }

        Ok(())
    }

    /// Orchestrates the semantic resolution loop, managing AI policy evaluation
    /// and SI stabilization retries.
    async fn resolve_semantic_mutation(
        &self,
        client_id: String,
        intent: &MutationIntent,
    ) -> Result<(MutationStatus, StabilizedMutation), Status> {
        let mut stabilized_mutation = None;
        let mut final_status = MutationStatus::Committed;

        for attempt in 0..=self.veto_max_retries {
            if attempt > 0 {
                info!(
                    "Retrying AI resolution (attempt {}/{})...",
                    attempt + 1,
                    self.veto_max_retries + 1
                );
            }

            let veto = match self.evaluate_policy(client_id.clone(), intent).await {
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
                final_status = MutationStatus::Vetoed;
                stabilized_mutation = Some(StabilizedMutation {
                    resolved_item_key: veto.resolved_item_key,
                    suggested_display_name: veto.suggested_display_name,
                    updated_base_quantity: "0".to_string(), // Rejection has zero impact
                    base_unit: "units".to_string(),
                    display_unit: "units".to_string(),
                    category: GroceryCategory::AnomalousInputs,
                    moral_justification: veto.moral_justification,
                });
                break;
            }

            match self.validate_and_stabilize(intent, &veto, &[]) {
                Ok(s) => {
                    stabilized_mutation = Some(s);
                    break;
                }
                Err(status) if attempt < self.veto_max_retries => {
                    warn!(
                        "AI response failed physical validation on attempt {}: {}. Retrying...",
                        attempt + 1,
                        status.message()
                    );
                    continue;
                }
                Err(status) => {
                    warn!(
                        "AI resolution exhausted retries and failed validation: {}",
                        status.message()
                    );
                    final_status = MutationStatus::Vetoed;
                    stabilized_mutation = Some(StabilizedMutation {
                        resolved_item_key: veto.resolved_item_key,
                        suggested_display_name: veto.suggested_display_name,
                        updated_base_quantity: "0".to_string(),
                        base_unit: "units".to_string(),
                        display_unit: "units".to_string(),
                        category: GroceryCategory::AnomalousInputs,
                        moral_justification: "Semantic Integrity Violation: AI-resolved metadata \
                                              failed internal verification."
                            .to_string(),
                    });
                    break;
                }
            }
        }

        let stabilized = stabilized_mutation
            .ok_or_else(|| self.internal_error("Retry loop failed to produce an outcome record"))?;

        Ok((final_status, stabilized))
    }

    /// Commits the finalized intent to the consensus log and waits for quorum
    /// commitment.
    #[allow(clippy::too_many_arguments)]
    async fn commit_to_consensus(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
        intent: MutationIntent,
        stabilized: StabilizedMutation,
        raw_user_input: String,
        status: MutationStatus,
        consensus_status: &ConsensusStatus,
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
            status,
            std::time::SystemTime::now(),
        );

        let mut command = Vec::new();
        mutation
            .encode(&mut command)
            .map_err(|e| self.internal_error(e.to_string()))?;

        let proposal_index = self
            .raft_handle
            .propose(command)
            .await
            .map_err(|e| self.map_consensus_error(e, consensus_status))?;

        info!(
            "Mutation index {} appended. Waiting for quorum...",
            proposal_index
        );

        self.raft_handle
            .await_commit(proposal_index)
            .await
            .map_err(|e| self.map_consensus_error(e, consensus_status))?;
        Ok(proposal_index)
    }

    /// Constructs the final gRPC response reflecting the committed lifecycle
    /// status.
    fn build_mutation_response(
        &self,
        index: LogIndex,
        status: MutationStatus,
        moral_justification: String,
    ) -> Response<ProposeMutationResponse> {
        Response::new(ProposeMutationResponse {
            status: status as i32,
            state_version: index.value(),
            leader_hint: String::new(),
            error_message: if status == MutationStatus::Vetoed {
                moral_justification
            } else {
                String::new()
            },
        })
    }

    // --- Implementation Detail Helpers ---

    /// Generates a logical rejection response containing a leader hint for
    /// redirection.
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

    /// Audits AI-resolved metadata against system registries and stabilizes
    /// physical quantities.
    fn validate_and_stabilize(
        &self,
        intent: &MutationIntent,
        veto: &VetoOutcome,
        current_inventory: &[common::proto::v1::app::GroceryItem],
    ) -> Result<StabilizedMutation, Status> {
        let category = self.verify_category_registry(&veto.category_assignment)?;

        if intent.operation == OperationType::Delete as i32 {
            return Ok(StabilizedMutation {
                resolved_item_key: veto.resolved_item_key.clone(),
                suggested_display_name: veto.suggested_display_name.clone(),
                updated_base_quantity: "0".to_string(),
                base_unit: "units".to_string(),
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

    /// Verifies AI-resolved categories against the clinical registry.
    fn verify_category_registry(&self, category_str: &str) -> Result<GroceryCategory, Status> {
        GroceryCategory::from_str(category_str).map_err(|_| {
            self.internal_error(format!(
                "AI Hallucination: Unregistered category '{}'",
                category_str
            ))
        })
    }

    /// Stabilizes user-provided units and quantities to their SI base
    /// representations.
    fn verify_unit_stabilization(
        &self,
        quantity: &str,
        unit_symbol: &str,
        ai_multiplier: &str,
    ) -> Result<PhysicalQuantity, Status> {
        let entry = UnitRegistry::resolve_symbol(unit_symbol).map_err(|e| {
            self.invalid_argument(format!(
                "Physical Invariant Violation: Invalid unit '{}' ({}).",
                unit_symbol, e
            ))
        })?;

        let ai_val = Decimal::from_str(ai_multiplier).map_err(|_| {
            self.internal_error(format!(
                "AI Hallucination: Malformed multiplier '{}' for contextual unit.",
                ai_multiplier
            ))
        })?;

        let base_quantity_res = if entry.is_contextual {
            UnitRegistry::parse_and_convert_with_multiplier(quantity, unit_symbol, ai_val)
        } else {
            UnitRegistry::parse_and_convert(quantity, unit_symbol)
        };

        let base_quantity = base_quantity_res.map_err(|e| {
            self.invalid_argument(format!(
                "Physical Invariant Violation: Stabilization failed ({}).",
                e
            ))
        })?;

        if base_quantity.value().is_sign_negative() || base_quantity.value().is_zero() {
            return Err(self.invalid_argument(
                "Physical Invariant Violation: Stabilized quantity must be strictly positive.",
            ));
        }

        Ok(base_quantity)
    }

    /// Captures the raw human intent for audit logging.
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

    /// Enforces the Dimensional Fence to prevent cross-dimensional arithmetic.
    fn enforce_physical_invariants(
        &self,
        intent: &MutationIntent,
        resolved_key: &str,
        new_quantity: &PhysicalQuantity,
        current_inventory: &[common::proto::v1::app::GroceryItem],
    ) -> Result<(), Status> {
        if (intent.operation == OperationType::Add as i32
            || intent.operation == OperationType::Subtract as i32)
            && let Some(existing_item) = current_inventory
                .iter()
                .find(|i| i.item_key == resolved_key)
        {
            let existing_unit = UnitRegistry::resolve_symbol(&existing_item.unit).map_err(|e| {
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
        Ok(())
    }

    /// Executes the AI policy evaluation with timeout and error handling.
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

    /// Translates domain ConsensusErrors into standard gRPC Status objects.
    fn map_consensus_error(&self, err: ConsensusError, status: &ConsensusStatus) -> Status {
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
                error!("CRITICAL: Node is in a poisoned state due to fatal safety violation.");
                Status::aborted("Node is in a fatal state and cannot process requests.")
            }
            ConsensusError::Internal(msg) => {
                error!("Consensus Internal Error: {}", msg);
                Status::internal("Internal Consensus Failure")
            }
            ConsensusError::Terminated => Status::unavailable("Consensus engine is shutting down"),
        }
    }

    /// Generates a standard gRPC InvalidArgument status.
    fn invalid_argument(&self, msg: impl Into<String>) -> Status {
        Status::invalid_argument(msg)
    }

    /// Generates a standard gRPC Internal error status.
    fn internal_error(&self, msg: impl Into<String>) -> Status {
        Status::internal(msg)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use common::proto::v1::app::GroceryItem;
    use common::proto::v1::app::MutationIntent;
    use common::proto::v1::app::MutationStatus;
    use common::proto::v1::app::OperationType;
    use common::proto::v1::app::ProposeMutationRequest;
    use common::proto::v1::app::QueryStateRequest;
    use common::proto::v1::app::QueryStatus;
    use common::proto::v1::app::SessionRecord;
    use common::raft_api::ConsensusHandle;
    use common::raft_api::ConsensusStatus;
    use common::raft_api::InventoryReader;
    use common::raft_api::SessionProvider;
    use common::types::ClientId;
    use common::types::LogIndex;
    use common::types::SequenceId;
    use common::types::errors::ConsensusError;
    use common::types::errors::FsmError;
    use prost::Message;
    use tonic::Request;

    use super::*;

    #[derive(Debug, Default)]
    struct MockRaftHandle {
        is_leader: bool,
        commit_index: LogIndex,
        leader_hint: String,
        rejection_reason: String,
        proposals: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl ConsensusHandle for MockRaftHandle {
        async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
            if self.is_leader {
                self.proposals.lock().unwrap().push(data);
                Ok(LogIndex::new(1))
            } else {
                Err(ConsensusError::NotLeader)
            }
        }

        async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
            if self.is_leader {
                Ok(())
            } else {
                Err(ConsensusError::NotLeader)
            }
        }

        async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
            Ok(())
        }

        async fn consensus_status(&self) -> ConsensusStatus {
            ConsensusStatus {
                is_leader: self.is_leader,
                commit_index: self.commit_index,
                leader_hint: self.leader_hint.clone(),
                rejection_reason: self.rejection_reason.clone(),
            }
        }

        async fn verify_leadership(&self) -> Result<(), ConsensusError> {
            if self.is_leader {
                Ok(())
            } else {
                Err(ConsensusError::NotLeader)
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
            _current_inventory: &[GroceryItem],
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
            _current_inventory: &[GroceryItem],
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
            _current_inventory: &[GroceryItem],
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
            _current_inventory: &[GroceryItem],
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
        items: Vec<GroceryItem>,
        version: LogIndex,
    }

    #[async_trait]
    impl SessionProvider for MockInventorySource {
        async fn check_session(
            &self,
            _client_id: &ClientId,
            _sequence_id: SequenceId,
        ) -> Result<Option<SessionRecord>, FsmError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl InventoryReader for MockInventorySource {
        async fn get_inventory(&self) -> Vec<GroceryItem> {
            self.items.clone()
        }

        async fn current_version(&self) -> LogIndex {
            self.version
        }
    }

    fn mock_dispatcher(
        raft_handle: Arc<dyn ConsensusHandle>,
        session_provider: Arc<dyn SessionProvider>,
        inventory_reader: Arc<dyn InventoryReader>,
        veto_relay: Arc<dyn VetoRelay>,
    ) -> IngressDispatcher {
        IngressDispatcher::new(
            raft_handle,
            session_provider,
            inventory_reader,
            veto_relay,
            Duration::from_secs(1),
            1,
            512,
        )
    }

    fn successful_raft() -> Arc<MockRaftHandle> {
        Arc::new(MockRaftHandle {
            is_leader: true,
            ..Default::default()
        })
    }

    fn successful_inventory() -> Arc<MockInventorySource> {
        Arc::new(MockInventorySource::default())
    }

    fn successful_veto() -> Arc<MockVetoRelay> {
        Arc::new(MockVetoRelay {
            outcome: Some(valid_outcome()),
            ..Default::default()
        })
    }

    mod propose_mutation {
        use super::*;

        // --- Phase 0: Leadership Authority ---

        #[tokio::test]
        async fn returns_rejected_when_not_leader() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: false,
                leader_hint: "http://leader:50051".to_string(),
                rejection_reason: "Node is a Follower".to_string(),
                ..Default::default()
            });
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
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
            }
            #[async_trait]
            impl ConsensusHandle for DuplicateRaft {
                async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                    self.mock.propose(data).await
                }

                async fn await_commit(&self, index: LogIndex) -> Result<(), ConsensusError> {
                    self.mock.await_commit(index).await
                }

                async fn await_apply(&self, index: LogIndex) -> Result<(), ConsensusError> {
                    self.mock.await_apply(index).await
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    self.mock.consensus_status().await
                }

                async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                    self.mock.verify_leadership().await
                }
            }

            #[derive(Debug)]
            struct DuplicateSource {
                committed_index: LogIndex,
            }
            #[async_trait]
            impl SessionProvider for DuplicateSource {
                async fn check_session(
                    &self,
                    cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(Some(SessionRecord::new(
                        cid,
                        SequenceId::new(1),
                        MutationStatus::Committed,
                        self.committed_index,
                        String::new(),
                        prost_types::Timestamp::default(),
                    )))
                }
            }
            #[async_trait]
            impl InventoryReader for DuplicateSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let committed_index = LogIndex::new(42);
            let raft = Arc::new(DuplicateRaft {
                mock: successful_raft(),
            });
            let inventory = Arc::new(DuplicateSource { committed_index });

            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
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
                successful_inventory(),
                successful_inventory(),
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
                successful_inventory(),
                successful_inventory(),
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
                sequence_id: 1,
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
            let dispatcher = {
                let inventory = successful_inventory();
                mock_dispatcher(
                    successful_raft(),
                    inventory.clone(),
                    inventory,
                    successful_veto(),
                )
            };
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
                successful_inventory(),
                successful_inventory(),
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
                sequence_id: 1,
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
            let dispatcher = {
                let inventory = successful_inventory();
                mock_dispatcher(
                    successful_raft(),
                    inventory.clone(),
                    inventory,
                    successful_veto(),
                )
            };
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
            let dispatcher = {
                let inventory = successful_inventory();
                mock_dispatcher(
                    successful_raft(),
                    inventory.clone(),
                    inventory,
                    successful_veto(),
                )
            };
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
            let dispatcher = {
                let inventory = successful_inventory();
                mock_dispatcher(
                    successful_raft(),
                    inventory.clone(),
                    inventory,
                    successful_veto(),
                )
            };
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

            let dispatcher = Arc::new({
                let inventory = successful_inventory();
                mock_dispatcher(raft, inventory.clone(), inventory, veto)
            });

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
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(successful_raft(), inventory.clone(), inventory, veto);
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
        async fn proposes_to_consensus_even_when_ai_rejects() {
            let raft = successful_raft();
            let veto = Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: false,
                    moral_justification: "Rejected by AI".to_string(),
                    ..valid_outcome()
                }),
                ..Default::default()
            });
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft.clone(), inventory.clone(), inventory, veto);

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

            // 1. Verify client receives Vetoed status
            assert_eq!(response.status, MutationStatus::Vetoed as i32);

            // 2. Verify consensus was still reached (Unified Ledger mandate)
            let proposals = raft.proposals.lock().unwrap();
            assert_eq!(
                proposals.len(),
                1,
                "Vetoed mutation was NOT proposed to consensus!"
            );

            let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
            assert_eq!(mutation.status, MutationStatus::Vetoed as i32);
            assert_eq!(mutation.moral_justification, "Rejected by AI");
        }

        #[tokio::test]
        async fn returns_error_on_ai_timeout() {
            let veto = Arc::new(MockVetoRelay {
                error: Some(VetoError::Timeout(Duration::from_secs(1))),
                ..Default::default()
            });
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(successful_raft(), inventory.clone(), inventory, veto);
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
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
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
            assert!(
                response
                    .error_message
                    .contains("Semantic Integrity Violation:")
            );
        }

        #[tokio::test]
        async fn returns_vetoed_when_ai_provides_invalid_conversion() {
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
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
                    .contains("Semantic Integrity Violation:")
            );
        }

        #[tokio::test]
        async fn returns_error_on_consensus_failure() {
            #[derive(Debug, Default)]
            struct FailingRaft;
            #[async_trait]
            impl ConsensusHandle for FailingRaft {
                async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                    Err(ConsensusError::Internal("Consensus failure".to_string()))
                }

                async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                    Ok(())
                }

                async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                    Ok(())
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    ConsensusStatus {
                        is_leader: true,
                        ..Default::default()
                    }
                }

                async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                    Ok(())
                }
            }

            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(
                Arc::new(FailingRaft),
                inventory.clone(),
                inventory,
                successful_veto(),
            );
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
            let inventory = successful_inventory();
            // Configured for 1 retry (max 2 attempts)
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                inventory.clone(),
                inventory,
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
            let inventory = successful_inventory();
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                inventory.clone(),
                inventory,
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
            let inventory = successful_inventory();
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                inventory.clone(),
                inventory,
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
            assert!(
                response
                    .error_message
                    .contains("Semantic Integrity Violation:")
            );
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
            let inventory = successful_inventory();
            // Configured for 1 retry (2 attempts total)
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                inventory.clone(),
                inventory,
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
            assert!(
                response
                    .error_message
                    .contains("Semantic Integrity Violation:")
            );
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
            let inventory = successful_inventory();
            let dispatcher = IngressDispatcher::new(
                raft.clone(),
                inventory.clone(),
                inventory,
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
            let inventory = successful_inventory();
            mock_dispatcher(
                Arc::new(MockRaftHandle::default()),
                inventory.clone(),
                inventory,
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

    mod exactly_once_semantics {
        use common::proto::v1::app::SessionRecord;

        use super::*;

        #[tokio::test]
        async fn returns_cached_outcome_when_sequence_is_duplicate() {
            let raft = successful_raft();
            let sid = SequenceId::new(42);
            let committed_index = LogIndex::new(100);

            #[derive(Debug, Default)]
            struct MockSource {
                record: Option<SessionRecord>,
            }
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    _cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(self.record.clone())
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let inventory = Arc::new(MockSource {
                record: Some(SessionRecord::new(
                    &ClientId::generate(), // Dummy for duplicate check
                    sid,
                    MutationStatus::Committed,
                    committed_index,
                    "Approved".to_string(),
                    prost_types::Timestamp::default(),
                )),
            });

            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: sid.value(),
                intent: Some(MutationIntent::default()),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Committed as i32);
            assert_eq!(response.state_version, committed_index.value());
        }

        #[tokio::test]
        async fn replays_cached_veto_with_justification() {
            let raft = successful_raft();
            let sid = SequenceId::new(42);

            #[derive(Debug, Default)]
            struct MockSource {
                record: Option<SessionRecord>,
            }
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    _cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(self.record.clone())
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let inventory = Arc::new(MockSource {
                record: Some(SessionRecord::new(
                    &ClientId::generate(),
                    sid,
                    MutationStatus::Vetoed,
                    LogIndex::new(10),
                    "Reason".to_string(),
                    prost_types::Timestamp::default(),
                )),
            });

            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: sid.value(),
                intent: Some(MutationIntent::default()),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Vetoed as i32);
            assert_eq!(response.error_message, "Reason");
        }

        #[tokio::test]
        async fn rejects_request_when_sequence_is_stale() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource;
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(Some(SessionRecord::new(
                        cid,
                        SequenceId::new(10),
                        MutationStatus::Committed,
                        LogIndex::ZERO,
                        String::new(),
                        prost_types::Timestamp::default(),
                    )))
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 5, // Stale: cluster is at 10
                intent: Some(MutationIntent::default()),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(
                response
                    .error_message
                    .contains("Secure Clinical: Stale Sequence")
            );
            assert!(!response.error_message.contains("10")); // Opaque check
        }

        #[tokio::test]
        async fn rejects_request_when_sequence_gap_detected() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource;
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(Some(SessionRecord::new(
                        cid,
                        SequenceId::new(1),
                        MutationStatus::Committed,
                        LogIndex::ZERO,
                        String::new(),
                        prost_types::Timestamp::default(),
                    )))
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 3, // Gap: expecting 2
                intent: Some(MutationIntent::default()),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(
                response
                    .error_message
                    .contains("Secure Clinical: Sequence Continuity Violation")
            );
            assert!(!response.error_message.contains("2")); // Opaque check
        }

        #[tokio::test]
        async fn rejects_request_when_new_client_skips_start_sequence() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource; // Returns None for check_session
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    _cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(None)
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 5, // Should be 1
                intent: Some(MutationIntent {
                    item_key: "milk".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(
                response
                    .error_message
                    .contains("Secure Clinical: Session Initialization Violation")
            );
        }

        #[tokio::test]
        async fn accepts_request_when_new_client_starts_at_sequence_one() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource;
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    _cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(None) // New client
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 1, // Correct start
                intent: Some(MutationIntent {
                    item_key: "item".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Committed as i32);
        }

        #[tokio::test]
        async fn accepts_request_when_sequence_is_next_in_line() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource;
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(Some(SessionRecord::new(
                        cid,
                        SequenceId::new(1),
                        MutationStatus::Committed,
                        LogIndex::ZERO,
                        String::new(),
                        prost_types::Timestamp::default(),
                    )))
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }

            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 2, // Valid: 1 + 1
                intent: Some(MutationIntent {
                    item_key: "item".to_string(),
                    quantity: Some("1".to_string()),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Committed as i32);
        }

        #[tokio::test]
        async fn rejects_request_when_sequence_id_is_zero() {
            let raft = successful_raft();
            #[derive(Debug, Default)]
            struct MockSource;
            #[async_trait]
            impl SessionProvider for MockSource {
                async fn check_session(
                    &self,
                    _cid: &ClientId,
                    _sid: SequenceId,
                ) -> Result<Option<SessionRecord>, FsmError> {
                    Ok(None)
                }
            }
            #[async_trait]
            impl InventoryReader for MockSource {
                async fn get_inventory(&self) -> Vec<GroceryItem> {
                    vec![]
                }

                async fn current_version(&self) -> LogIndex {
                    LogIndex::ZERO
                }
            }
            let mock_source = Arc::new(MockSource);
            let dispatcher =
                mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
            let req = Request::new(ProposeMutationRequest {
                client_id: ClientId::generate().as_str().to_string(),
                sequence_id: 0, // Forbidden by protocol
                intent: Some(MutationIntent::default()),
            });

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
            assert_eq!(response.status, MutationStatus::Rejected as i32);
            assert!(
                response
                    .error_message
                    .contains("Secure Clinical: Protocol Violation")
            );
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
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
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
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Success as i32);
            assert_eq!(response.items.len(), 2);

            let mut response_items = response.items;
            response_items.sort_by_key(|i| i.item_key.clone());
            assert_eq!(response_items[0].item_key, "eggs");
            assert_eq!(response_items[1].item_key, "milk");
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
                ..Default::default()
            });
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

            let req = Request::new(QueryStateRequest {
                query_filter: Some("milk".to_string()),
                min_state_version: None,
            });

            let response = dispatcher.query_state(req).await.unwrap().into_inner();
            assert_eq!(response.status, QueryStatus::Success as i32);

            let mut response_items = response.items;
            response_items.sort_by_key(|i| i.item_key.clone());

            assert_eq!(response_items.len(), 2);
            assert_eq!(response_items[0].item_key, "milk-skim");
            assert_eq!(response_items[1].item_key, "milk-whole");
        }

        #[tokio::test]
        async fn rejects_query_when_await_apply_fails() {
            #[derive(Debug, Default)]
            struct FailingApplyRaft;
            #[async_trait]
            impl ConsensusHandle for FailingApplyRaft {
                async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                    Ok(LogIndex::new(1))
                }

                async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                    Ok(())
                }

                async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                    Err(ConsensusError::Poisoned)
                }

                async fn consensus_status(&self) -> ConsensusStatus {
                    ConsensusStatus {
                        is_leader: true,
                        commit_index: LogIndex::new(100),
                        ..Default::default()
                    }
                }

                async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                    Ok(())
                }
            }

            let raft = Arc::new(FailingApplyRaft);
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: Some(10),
            });

            let result = dispatcher.query_state(req).await;
            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::Aborted);
            assert!(status.message().contains("fatal state"));
        }

        #[tokio::test]
        async fn rejects_query_exceeding_horizon() {
            let raft = Arc::new(MockRaftHandle {
                is_leader: true,
                commit_index: LogIndex::new(5),
                ..Default::default()
            });
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

            let req = Request::new(QueryStateRequest {
                query_filter: None,
                min_state_version: Some(10), // Exceeds horizon (5)
            });

            let result = dispatcher.query_state(req).await;
            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert!(status.message().contains("exceeds consistent horizon"));
        }
    }
}
