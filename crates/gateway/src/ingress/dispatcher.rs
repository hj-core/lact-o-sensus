use std::fmt::Debug;
use std::sync::Arc;

use common::app_api::InventoryReader;
use common::app_api::SessionProvider;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_server::IngressService;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::types::errors::ConsensusError;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use common::units::UnitRegistry;
use common_rpc::TraceInterceptor;
use raft_engine::ConsensusAuthority;
use raft_engine::ConsensusHandle;
use tokio::sync::Mutex;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::info;
use tracing::info_span;

use super::ai_oracle;
use super::proposer;
use super::scrubber;
use super::sequencer;
use super::types::AuthorityOutcome;
use super::types::IngressConfig;
use super::types::MutationProposal;
use super::types::StabilizedMutation;
use crate::veto::VetoRelay;

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
    config: IngressConfig,
    /// Mutex serving as the Layer 2 MutationLock (ADR 007).
    /// Ensures that AI evaluation and proposal happen sequentially on the
    /// leader.
    mutation_lock: Mutex<()>,
}

impl IngressDispatcher {
    pub fn new(
        raft_handle: Arc<dyn ConsensusHandle>,
        session_provider: Arc<dyn SessionProvider>,
        inventory_reader: Arc<dyn InventoryReader>,
        veto_relay: Arc<dyn VetoRelay>,
        config: IngressConfig,
    ) -> Self {
        Self {
            raft_handle,
            session_provider,
            inventory_reader,
            veto_relay,
            config,
            mutation_lock: Mutex::new(()),
        }
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
        let trace_id = TraceInterceptor::require_trace_id(&request)?;

        let mut result = self.handle_propose_mutation(request, trace_id).await;

        if let Ok(ref mut response) = result {
            TraceInterceptor::inject_trace_id_into_response(response, trace_id)?;
        }
        result
    }

    /// Queries the current linearizable grocery state.
    async fn query_state(
        &self,
        request: Request<QueryStateRequest>,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let trace_id = TraceInterceptor::require_trace_id(&request)?;

        let mut result = self.handle_query_state(request, trace_id).await;

        if let Ok(ref mut response) = result {
            TraceInterceptor::inject_trace_id_into_response(response, trace_id)?;
        }
        result
    }
}

impl IngressDispatcher {
    /// Internal handler for mutations, executed within the TraceId scope.
    async fn handle_propose_mutation(
        &self,
        request: Request<ProposeMutationRequest>,
        trace_id: TraceId,
    ) -> Result<Response<ProposeMutationResponse>, Status> {
        let req = request.into_inner();

        let sequence_id = SequenceId::new(req.sequence_id);
        let client_id = req
            .client_id
            .parse::<ClientId>()
            .map_err(|e| self.invalid_argument(format!("Invalid client_id: {}", e)))?;

        let span = info_span!(
            "propose_mutation",
            trace_id = %trace_id,
            client = %client_id.truncated(),
            seq = %sequence_id
        );
        let _enter = span.enter();

        // 1. Verifies that this node is the authorized leader.
        let status = match self.authorize_mutation() {
            AuthorityOutcome::Authorized(s) => s,
            AuthorityOutcome::Redirect(r) => return Ok(r),
            AuthorityOutcome::Fatal(e) => return Err(e),
        };

        // 2. Enforces linearizability via the sequence firewall.
        if let Some(cached_response) = self
            .enforce_sequence_firewall(&client_id, sequence_id)
            .await?
        {
            return Ok(cached_response);
        }

        // 3. Guards against concurrent mutation attempts and instruments the critical
        //    section.
        let sequencing_span = info_span!(
            target: ClinicalTarget::ClinicalIngress.as_str(),
            "mutation_sequencing",
            %trace_id,
            client = %client_id.truncated()
        );

        async {
            let _lock = self.mutation_lock.lock().await;

            // 4. Scrubs user input and ensures syntactic/taxonomic integrity.
            let mut intent = req.intent.clone().ok_or_else(|| {
                self.invalid_argument("ProposeMutationRequest is missing 'intent' field")
            })?;
            let raw_user_input = self.format_raw_input(&intent);

            tracing::trace!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                raw_input = %raw_user_input,
                "Raw User Input (PII)"
            );

            self.normalize_intent(&mut intent)?;

            // 5. Fetches the authoritative linearizable state for context (ADR 007).
            let current_inventory = self.inventory_reader.get_inventory();

            // 6. Resolves semantic metadata and stabilizes physical quantities via the AI
            //    resolution loop.
            let (final_status, stabilized) = self
                .resolve_semantic_mutation(client_id.clone(), &intent, &current_inventory, trace_id)
                .await?;

            // 6.5 Re-checks authority — the leader may have been demoted during
            //     AI evaluation (long-running LLM call). If so, redirect the client
            //     to the new leader instead of attempting to propose to a stale
            //     term.
            let fresh = self.raft_handle.authority();
            if !fresh.is_leader {
                return Ok(self.mutation_redirection_response(fresh));
            }

            // 7. Proposes the finalized intent to the cluster for consensus.
            let proposal_index = self
                .commit_to_consensus(MutationProposal {
                    client_id: &client_id,
                    sequence_id,
                    intent,
                    stabilized: stabilized.clone(),
                    raw_user_input,
                    status: final_status,
                    consensus_status: &status,
                })
                .await?;

            info!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                index = %proposal_index,
                status = ?final_status,
                category_slug = %stabilized.category,
                item_slug = %stabilized.resolved_item_key,
                "Mutation committed to consensus."
            );

            // 7. Constructs the final response reflecting the committed lifecycle status.
            Ok(self.build_mutation_response(
                proposal_index,
                final_status,
                stabilized.moral_justification,
            ))
        }
        .instrument(sequencing_span)
        .await
    }

    /// Internal handler for queries, executed within the TraceId scope.
    async fn handle_query_state(
        &self,
        request: Request<QueryStateRequest>,
        trace_id: TraceId,
    ) -> Result<Response<QueryStateResponse>, Status> {
        let req = request.into_inner();

        let span = info_span!("query_state", trace_id = %trace_id);
        let _enter = span.enter();

        // 1. Verifies that this node is the authorized leader.
        let status = match self.authorize_query() {
            AuthorityOutcome::Authorized(s) => s,
            AuthorityOutcome::Redirect(r) => return Ok(r),
            AuthorityOutcome::Fatal(e) => return Err(e),
        };

        // 2. Linearizable Quorum Read (Verification of leadership continuity).
        if let Err(e) = self.raft_handle.verify_leadership().await {
            match e {
                ConsensusError::NotLeader => {
                    return Ok(self.query_redirection_response(status));
                }
                _ => return Err(Status::internal(format!("Linearizable read failed: {}", e))),
            }
        }

        // 3. Linearizable Consistency Fence (ADR 006).
        // If a minimum state version is requested, we ensure the local state
        // machine has caught up to that version before responding.
        if let Some(version) = req.min_state_version {
            if version > status.last_committed.as_u64() {
                return Err(Status::failed_precondition(format!(
                    "Requested version {} exceeds consistent horizon {}.",
                    version,
                    status.last_committed.as_u64()
                )));
            }

            tokio::time::timeout(
                self.config.consensus_timeout,
                self.raft_handle.await_apply(LogIndex::new(version)),
            )
            .await
            .map_err(|_| {
                Status::deadline_exceeded(format!(
                    "Consistency fence at version {} timed out after {:?}",
                    version, self.config.consensus_timeout
                ))
            })?
            .map_err(|e| self.map_consensus_error(e, &status))?;
        }

        // 4. Fetches the consolidated inventory from the State Machine.
        let items = self.inventory_reader.get_inventory();

        // 4b. Display Conversion (ADR 008): Convert SI base quantities to the
        // user's preferred display unit when possible.
        let items: Vec<GroceryItem> = items
            .into_iter()
            .map(|mut item| {
                if !item.display_unit.is_empty()
                    && item.display_unit != item.unit
                    && let Some(display_qty) =
                        UnitRegistry::convert_to_display_value(&item.quantity, &item.display_unit)
                {
                    item.quantity = display_qty;
                    item.unit = item.display_unit.clone();
                }
                item
            })
            .collect();

        let version = self.inventory_reader.current_version();

        // 5. Redacts or filters results if a query filter was provided.
        let filtered_items = if let Some(ref filter) = req.query_filter {
            let filter_lower = filter.to_lowercase();
            items
                .into_iter()
                .filter(|item| item.item_key.to_lowercase().contains(&filter_lower))
                .collect()
        } else {
            items
        };

        Ok(Response::new(QueryStateResponse {
            items: filtered_items,
            current_state_version: version.as_u64(),
            status: QueryStatus::Success as i32,
            ..Default::default()
        }))
    }
}

impl IngressDispatcher {
    // --- Implementation Detail Helpers ---

    /// Orchestrates the authority check for mutation requests.
    fn authorize_mutation(&self) -> AuthorityOutcome<ProposeMutationResponse> {
        let status = self.raft_handle.authority();
        if status.is_poisoned {
            return AuthorityOutcome::Fatal(self.poisoned_node_error());
        }
        if !status.is_leader {
            return AuthorityOutcome::Redirect(self.mutation_redirection_response(status));
        }
        AuthorityOutcome::Authorized(status)
    }

    /// Orchestrates the authority check for query requests.
    fn authorize_query(&self) -> AuthorityOutcome<QueryStateResponse> {
        let status = self.raft_handle.authority();
        if status.is_poisoned {
            return AuthorityOutcome::Fatal(self.poisoned_node_error());
        }
        if !status.is_leader {
            return AuthorityOutcome::Redirect(self.query_redirection_response(status));
        }
        AuthorityOutcome::Authorized(status)
    }

    /// Generates a redirection response for a mutation request.
    fn mutation_redirection_response(
        &self,
        status: ConsensusAuthority,
    ) -> Response<ProposeMutationResponse> {
        Response::new(ProposeMutationResponse {
            status: MutationStatus::Rejected as i32,
            state_version: 0,
            leader_hint: status.leader_hint,
            error_message: status.rejection_reason,
        })
    }

    /// Generates a redirection response for a query request.
    fn query_redirection_response(
        &self,
        status: ConsensusAuthority,
    ) -> Response<QueryStateResponse> {
        Response::new(QueryStateResponse {
            status: QueryStatus::Rejected as i32,
            current_state_version: 0,
            leader_hint: status.leader_hint,
            error_message: status.rejection_reason,
            ..Default::default()
        })
    }

    /// Delegates to the sequencer submodule for exactly-once semantics
    /// enforcement.
    async fn enforce_sequence_firewall(
        &self,
        client_id: &ClientId,
        sequence_id: SequenceId,
    ) -> Result<Option<Response<ProposeMutationResponse>>, Status> {
        sequencer::enforce_sequence_firewall(&*self.session_provider, client_id, sequence_id).await
    }

    /// Delegates to the scrubber submodule for syntactic/taxonomic
    /// normalization.
    fn normalize_intent(&self, intent: &mut MutationIntent) -> Result<(), Status> {
        scrubber::normalize_intent(intent)
    }

    /// Delegates to the ai_oracle submodule for semantic resolution.
    async fn resolve_semantic_mutation(
        &self,
        client_id: ClientId,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
        trace_id: TraceId,
    ) -> Result<(MutationStatus, StabilizedMutation), Status> {
        ai_oracle::resolve_semantic_mutation(
            &*self.veto_relay,
            &self.config,
            client_id,
            intent,
            current_inventory,
            trace_id,
        )
        .await
    }

    /// Delegates to the proposer submodule for consensus commitment.
    async fn commit_to_consensus(
        &self,
        proposal: MutationProposal<'_>,
    ) -> Result<LogIndex, Status> {
        proposer::commit_to_consensus(&*self.raft_handle, self.config.consensus_timeout, proposal)
            .await
    }

    /// Delegates to the proposer submodule for mutation response construction.
    fn build_mutation_response(
        &self,
        index: LogIndex,
        status: MutationStatus,
        moral_justification: String,
    ) -> Response<ProposeMutationResponse> {
        proposer::build_mutation_response(index, status, moral_justification)
    }

    /// Delegates to the scrubber submodule for raw input formatting.
    fn format_raw_input(&self, intent: &MutationIntent) -> String {
        scrubber::format_raw_input(intent)
    }

    /// Delegates to the proposer submodule for consensus error translation.
    fn map_consensus_error(&self, err: ConsensusError, status: &ConsensusAuthority) -> Status {
        proposer::map_consensus_error(err, status)
    }

    /// Generates a standard gRPC InvalidArgument status.
    fn invalid_argument(&self, msg: impl Into<String>) -> Status {
        Status::invalid_argument(msg)
    }

    /// Generates a standard gRPC Internal error status.
    fn internal_error(&self, msg: impl Into<String>) -> Status {
        Status::internal(msg)
    }

    /// Generates a fatal gRPC error for poisoned nodes (Halt Mandate).
    fn poisoned_node_error(&self) -> Status {
        self.internal_error("Secure Clinical: Node is poisoned and cannot process requests")
    }
}
