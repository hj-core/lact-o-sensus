use std::time::Duration;

use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::taxonomy::GroceryCategory;
use common::types::ClientId;
use common::types::trace::ClinicalTarget;
use common::types::trace::TraceId;
use tonic::Status;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;

use super::stabilizer;
use super::types::IngressConfig;
use super::types::StabilizedMutation;
use crate::veto::VetoError;
use crate::veto::VetoOutcome;
use crate::veto::VetoRelay;

/// Executes the AI policy evaluation with timeout and error handling.
pub(crate) async fn evaluate_policy(
    veto_relay: &dyn VetoRelay,
    veto_timeout: Duration,
    max_justification_len: usize,
    client_id: ClientId,
    intent: &MutationIntent,
    current_inventory: &[GroceryItem],
    trace_id: TraceId,
) -> Result<VetoOutcome, Status> {
    let span = info_span!(
        target: ClinicalTarget::ClinicalOracle.as_str(),
        "veto_evaluation",
        %trace_id,
        timeout = ?veto_timeout
    );

    info!(
        target: ClinicalTarget::ClinicalIngress.as_str(),
        "Triggering AI Veto evaluation for normalized intent..."
    );

    let outcome = veto_relay
        .evaluate(
            client_id,
            intent,
            current_inventory,
            veto_timeout,
            max_justification_len,
            trace_id,
        )
        .instrument(span)
        .await;

    match outcome {
        Ok(v) => {
            if !v.is_approved {
                info!(
                    target: ClinicalTarget::ClinicalIngress.as_str(),
                    resolution = "Vetoed",
                    "Mutation VETOED by AI"
                );
                tracing::trace!(
                    target: ClinicalTarget::ClinicalIngress.as_str(),
                    moral_justification = %v.moral_justification,
                    "AI Moral Justification (PII)"
                );
            }
            Ok(v)
        }
        Err(VetoError::CausalIntegrityViolation) => {
            error!(
                target: ClinicalTarget::ClinicalTelemetry.as_str(),
                "Causal Integrity Violation: AI Veto Node returned mismatched TraceId"
            );
            Err(Status::failed_precondition("Causal Integrity Violation"))
        }
        Err(VetoError::Timeout(d)) => {
            warn!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                timeout = ?d,
                "AI Veto evaluation timed out"
            );
            Err(Status::deadline_exceeded(
                "AI evaluation timed out. Please retry shortly.",
            ))
        }
        Err(e) => {
            error!(
                target: ClinicalTarget::ClinicalIngress.as_str(),
                error = %e,
                "AI Veto infrastructure failure"
            );
            Err(Status::internal("Internal policy engine failure"))
        }
    }
}

/// Orchestrates the semantic resolution loop, managing AI policy evaluation
/// and SI stabilization retries.
pub(crate) async fn resolve_semantic_mutation(
    veto_relay: &dyn VetoRelay,
    config: &IngressConfig,
    client_id: ClientId,
    intent: &MutationIntent,
    current_inventory: &[GroceryItem],
    trace_id: TraceId,
) -> Result<(MutationStatus, StabilizedMutation), Status> {
    let mut stabilized_mutation = None;
    let mut final_status = MutationStatus::Committed;

    for attempt in 0..=config.veto_max_retries {
        if attempt > 0 {
            info!(
                target: ClinicalTarget::ClinicalOracle.as_str(),
                attempt = attempt + 1,
                max_retries = config.veto_max_retries + 1,
                "Retrying AI resolution..."
            );
        }

        let veto = match evaluate_policy(
            veto_relay,
            config.veto_timeout,
            config.max_justification_len,
            client_id.clone(),
            intent,
            current_inventory,
            trace_id,
        )
        .await
        {
            Ok(v) => v,
            Err(e) if attempt < config.veto_max_retries => {
                warn!(
                    target: ClinicalTarget::ClinicalOracle.as_str(),
                    attempt = attempt + 1,
                    error = %e,
                    "Transient AI failure. Retrying..."
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
                updated_base_quantity: "0".to_string(),
                base_unit: "units".to_string(),
                display_unit: "units".to_string(),
                category: GroceryCategory::AnomalousInputs,
                moral_justification: veto.moral_justification,
            });
            break;
        }

        match stabilizer::validate_and_stabilize(intent, &veto, current_inventory) {
            Ok(s) => {
                stabilized_mutation = Some(s);
                break;
            }
            Err(status) if attempt < config.veto_max_retries => {
                warn!(
                    target: ClinicalTarget::ClinicalOracle.as_str(),
                    attempt = attempt + 1,
                    error = %status.message(),
                    "AI response failed physical validation. Retrying..."
                );
                continue;
            }
            Err(status) => {
                warn!(
                    target: ClinicalTarget::ClinicalOracle.as_str(),
                    error = %status.message(),
                    "AI resolution exhausted retries and failed validation."
                );
                final_status = MutationStatus::Vetoed;
                stabilized_mutation = Some(StabilizedMutation {
                    resolved_item_key: veto.resolved_item_key,
                    suggested_display_name: veto.suggested_display_name,
                    updated_base_quantity: "0".to_string(),
                    base_unit: "units".to_string(),
                    display_unit: "units".to_string(),
                    category: GroceryCategory::AnomalousInputs,
                    moral_justification: "A physical unit mismatch was detected between this \
                                          request and the existing inventory record for this \
                                          item. Please ensure the units are compatible with \
                                          previous entries."
                        .to_string(),
                });
                break;
            }
        }
    }

    let stabilized = stabilized_mutation
        .ok_or_else(|| Status::internal("Retry loop failed to produce an outcome record"))?;

    Ok((final_status, stabilized))
}
