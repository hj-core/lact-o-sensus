use std::str::FromStr;

use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::OperationType;
use common::taxonomy::GroceryCategory;
use rust_decimal::Decimal;
use tonic::Status;

use super::types::Operation;
use super::types::ScrubbedIntent;

/// Normalizes user intents and enforces clinical taxonomy constraints
/// before semantic resolution. Returns a domain-typed `ScrubbedIntent`
/// decoupled from the proto representation.
pub(crate) fn normalize_intent(intent: &MutationIntent) -> Result<ScrubbedIntent, Status> {
    let item_key = intent.item_key.trim().to_lowercase();
    let mut quantity = intent.quantity.as_deref().map(str::trim).map(String::from);
    if quantity.as_deref() == Some("") {
        quantity = None;
    }
    if let Some(ref q) = quantity {
        let val = Decimal::from_str(q)
            .map_err(|_| Status::invalid_argument(format!("Invalid quantity format: '{}'", q)))?;
        if val.is_sign_negative() {
            return Err(Status::invalid_argument(
                "quantity cannot be negative. Use SUBTRACT or DELETE for removals.",
            ));
        }
    }

    let unit = intent
        .unit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let category = intent
        .category
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    // Taxonomy Guard (ADR 007 Layer 2)
    if let Some(ref cat) = category {
        GroceryCategory::from_str(cat).map_err(|_| {
            Status::invalid_argument(format!(
                "Invalid category hint: '{}'. Must be one of the 12 clinical categories.",
                cat
            ))
        })?;
    }

    if item_key.is_empty() {
        return Err(Status::invalid_argument("item_key cannot be empty"));
    }

    let operation = match OperationType::try_from(intent.operation) {
        Ok(OperationType::Add) => Operation::Add,
        Ok(OperationType::Subtract) => Operation::Subtract,
        Ok(OperationType::Set) => Operation::Set,
        Ok(OperationType::Delete) => Operation::Delete,
        _ => {
            return Err(Status::invalid_argument("Unknown operation type"));
        }
    };

    if operation == Operation::Delete && quantity.is_some() {
        return Err(Status::invalid_argument(
            "DELETE operations must not contain a quantity string",
        ));
    }

    if operation != Operation::Delete && quantity.is_none() {
        return Err(Status::invalid_argument(
            "quantity is required for this operation",
        ));
    }

    Ok(ScrubbedIntent {
        item_key,
        operation,
        quantity,
        unit,
        category,
    })
}

/// Captures the raw human intent for audit logging.
/// Operates on the proto MutationIntent to preserve the original user-typed
/// string before normalization.
pub(crate) fn format_raw_input_from_proto(intent: &MutationIntent) -> String {
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
