use std::str::FromStr;

use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::OperationType;
use common::taxonomy::GroceryCategory;
use common::units::PhysicalQuantity;
use common::units::UnitRegistry;
use rust_decimal::Decimal;
use tonic::Status;

use super::types::StabilizedMutation;
use crate::veto::VetoOutcome;

/// Audits AI-resolved metadata against system registries and stabilizes
/// physical quantities.
pub(crate) fn validate_and_stabilize(
    intent: &MutationIntent,
    veto: &VetoOutcome,
    current_inventory: &[GroceryItem],
) -> Result<StabilizedMutation, Status> {
    let category = verify_category_registry(&veto.category_assignment)?;

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
        .ok_or_else(|| Status::invalid_argument("quantity is missing"))?;

    let base_quantity = verify_unit_stabilization(
        q_str,
        &veto.resolved_unit,
        &veto.conversion_multiplier_to_base,
    )?;

    enforce_physical_invariants(
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
fn verify_category_registry(category_str: &str) -> Result<GroceryCategory, Status> {
    GroceryCategory::from_str(category_str).map_err(|_| {
        Status::internal(format!(
            "AI Hallucination: Unregistered category '{}'",
            category_str
        ))
    })
}

/// Stabilizes user-provided units and quantities to their SI base
/// representations.
fn verify_unit_stabilization(
    quantity: &str,
    unit_symbol: &str,
    ai_multiplier: &str,
) -> Result<PhysicalQuantity, Status> {
    let entry = UnitRegistry::resolve_symbol(unit_symbol).map_err(|e| {
        Status::invalid_argument(format!(
            "Physical Invariant Violation: Invalid unit '{}' ({}).",
            unit_symbol, e
        ))
    })?;

    let ai_val = Decimal::from_str(ai_multiplier).map_err(|_| {
        Status::internal(format!(
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
        Status::invalid_argument(format!(
            "Physical Invariant Violation: Stabilization failed ({}).",
            e
        ))
    })?;

    if base_quantity.value().is_sign_negative() || base_quantity.value().is_zero() {
        return Err(Status::invalid_argument(
            "Physical Invariant Violation: Stabilized quantity must be strictly positive.",
        ));
    }

    Ok(base_quantity)
}

/// Enforces the Dimensional Fence to prevent cross-dimensional arithmetic.
fn enforce_physical_invariants(
    intent: &MutationIntent,
    resolved_key: &str,
    new_quantity: &PhysicalQuantity,
    current_inventory: &[GroceryItem],
) -> Result<(), Status> {
    if (intent.operation == OperationType::Add as i32
        || intent.operation == OperationType::Subtract as i32)
        && let Some(existing_item) = current_inventory
            .iter()
            .find(|i| i.item_key == resolved_key)
    {
        let existing_unit = UnitRegistry::resolve_symbol(&existing_item.unit).map_err(|e| {
            Status::internal(format!(
                "Internal state corruption: Existing item has invalid unit '{}' ({})",
                existing_item.unit, e
            ))
        })?;

        if existing_unit.dimension != new_quantity.dimension() {
            return Err(Status::invalid_argument(format!(
                "Physical Invariant Violation: Cannot perform arithmetic between {:?} and {:?} \
                 (Dimensional Fence).",
                existing_unit.dimension,
                new_quantity.dimension()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use common::proto::v1::app::GroceryItem;
    use common::proto::v1::app::MutationIntent;
    use common::proto::v1::app::OperationType;
    use common::types::LogIndex;

    use super::validate_and_stabilize;
    use crate::ingress::test_utils::*;
    use crate::veto::VetoOutcome;

    #[test]
    fn rejects_hallucinated_category() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            category_assignment: "Space Matter".to_string(), // Hallucination
            moral_justification: "Approved".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "g".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };

        let result = validate_and_stabilize(&intent, &veto, &[]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
    }

    #[test]
    fn rejects_hallucinated_unit() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            category_assignment: "Primary Flora".to_string(),
            moral_justification: "Approved".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "blorgs".to_string(), // Hallucination
            conversion_multiplier_to_base: "1".to_string(),
        };

        let result = validate_and_stabilize(&intent, &veto, &[]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_invalid_si_unit_conversion() {
        let intent = MutationIntent::new(
            "".into(),
            Some("abc".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            category_assignment: "Primary Flora".to_string(),
            moral_justification: "Approved".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "g".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };

        let result = validate_and_stabilize(&intent, &veto, &[]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_cross_dimensional_arithmetic() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1".to_string()),
            None,
            None,
            OperationType::Add,
        );
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
        let inventory = vec![GroceryItem::new(
            "milk".to_string(),
            "0".to_string(),
            "g".to_string(),
            "Animal Secretions".to_string(),
            "client".to_string(),
            prost_types::Timestamp::default(),
            LogIndex::new(0),
        )];

        let result = validate_and_stabilize(&intent, &veto, &inventory);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Dimensional Fence"));
    }

    #[test]
    fn applies_bankers_rounding_to_si_stabilization() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1.5".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            category_assignment: "Primary Flora".to_string(),
            moral_justification: "Approved".to_string(),
            resolved_item_key: "item".to_string(),
            suggested_display_name: "Item".to_string(),
            resolved_unit: "lb".to_string(), // 1 lb = 453.59237 g
            conversion_multiplier_to_base: "453.59237".to_string(),
        };

        let result = validate_and_stabilize(&intent, &veto, &[]).unwrap();

        // 1.5 * 453.59237 = 680.388555
        // Banker's Rounding to 4 dp as defined in units.rs
        assert_eq!(result.updated_base_quantity, "680.3886");
        assert_eq!(result.base_unit, "g");
    }

    #[test]
    fn grants_contextual_override_when_unit_is_dynamic() {
        let intent = MutationIntent::new(
            "".into(),
            Some("2".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            resolved_unit: "pack".to_string(), // Contextual unit
            conversion_multiplier_to_base: "6".to_string(), // AI resolves 6 per pack
            ..valid_outcome()
        };

        let result = validate_and_stabilize(&intent, &veto, &[]).unwrap();
        // 2 packs * 6 multiplier = 12 base units
        assert_eq!(result.updated_base_quantity, "12");
    }

    #[test]
    fn ignores_physical_constant_redefinition_when_unit_is_static() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1".to_string()),
            None,
            None,
            OperationType::Add,
        );
        let veto = VetoOutcome {
            is_approved: true,
            resolved_unit: "kg".to_string(), // Static unit
            conversion_multiplier_to_base: "500".to_string(), /* AI attempts to redefine 1kg
                                              * = 500g */
            ..valid_outcome()
        };

        let result = validate_and_stabilize(&intent, &veto, &[]).unwrap();

        // Physical Law Check: Registry (1000) must override AI (500)
        assert_eq!(result.updated_base_quantity, "1000");
        assert_eq!(result.base_unit, "g");
    }

    #[test]
    fn rejects_non_positive_quantity_during_stabilization() {
        let intent = MutationIntent::new(
            "".into(),
            Some("1".to_string()),
            None,
            None,
            OperationType::Add,
        );

        // Test 1: Zero (using contextual unit to ensure AI multiplier is applied)
        let veto_zero = VetoOutcome {
            is_approved: true,
            resolved_unit: "pack".to_string(),
            conversion_multiplier_to_base: "0".to_string(),
            ..valid_outcome()
        };
        let status_zero = validate_and_stabilize(&intent, &veto_zero, &[]).unwrap_err();
        assert_eq!(status_zero.code(), tonic::Code::InvalidArgument);

        // Test 2: Negative
        let veto_neg = VetoOutcome {
            is_approved: true,
            resolved_unit: "pack".to_string(),
            conversion_multiplier_to_base: "-1".to_string(),
            ..valid_outcome()
        };
        let status_neg = validate_and_stabilize(&intent, &veto_neg, &[]).unwrap_err();
        assert_eq!(status_neg.code(), tonic::Code::InvalidArgument);
        assert!(status_neg.message().contains("strictly positive"));
    }
}
