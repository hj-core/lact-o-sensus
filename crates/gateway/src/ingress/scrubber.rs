use std::str::FromStr;

use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::OperationType;
use common::taxonomy::GroceryCategory;
use rust_decimal::Decimal;
use tonic::Status;

/// Normalizes user intents and enforces clinical taxonomy constraints
/// before semantic resolution.
pub(crate) fn normalize_intent(intent: &mut MutationIntent) -> Result<(), Status> {
    intent.item_key = intent.item_key.trim().to_lowercase();

    if let Some(q) = intent.quantity.as_mut() {
        let trimmed = q.trim();
        if trimmed.is_empty() {
            intent.quantity = None;
        } else {
            let val = Decimal::from_str(trimmed).map_err(|_| {
                Status::invalid_argument(format!("Invalid quantity format: '{}'", trimmed))
            })?;
            if val.is_sign_negative() {
                return Err(Status::invalid_argument(
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
                Status::invalid_argument(format!(
                    "Invalid category hint: '{}'. Must be one of the 12 clinical categories.",
                    trimmed
                ))
            })?;
            *category = trimmed.to_string();
        }
    }

    if intent.item_key.is_empty() {
        return Err(Status::invalid_argument("item_key cannot be empty"));
    }

    if intent.operation == OperationType::Delete as i32 && intent.quantity.is_some() {
        return Err(Status::invalid_argument(
            "DELETE operations must not contain a quantity string",
        ));
    }

    if intent.operation != OperationType::Delete as i32 && intent.quantity.is_none() {
        return Err(Status::invalid_argument(
            "quantity is required for this operation",
        ));
    }

    Ok(())
}

/// Captures the raw human intent for audit logging.
pub(crate) fn format_raw_input(intent: &MutationIntent) -> String {
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
