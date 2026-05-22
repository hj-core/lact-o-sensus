//! 12-Point Authorized Taxonomy for the Lact-O-Sensus cluster.
//!
//! This module defines the clinical classification system used for grocery
//! inventory. Every item in the ledger must map to exactly one category,
//! facilitating deterministic state transitions and providing a structured
//! domain for the AI Moral Advocate to evaluate.

use serde::Deserialize;
use serde::Serialize;
use strum::Display;
use strum::EnumString;

/// The 12-Point Authorized Taxonomy for grocery classification.
///
/// All grocery items must map to exactly one of these clinical categories to
/// ensure deterministic state transitions and provide metadata for the AI Moral
/// Advocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumString)]
#[strum(serialize_all = "PascalCase", ascii_case_insensitive)]
pub enum GroceryCategory {
    /// Unprocessed botanical matter (Fruits, Vegetables).
    #[strum(serialize = "Primary Flora", serialize = "PrimaryFlora")]
    PrimaryFlora,
    /// Liquid and solid animal-derived proteins (Dairy, Eggs).
    #[strum(serialize = "Animal Secretions", serialize = "AnimalSecretions")]
    AnimalSecretions,
    /// Carcass-based nutritional inputs (Meat, Seafood).
    #[strum(serialize = "Flesh And Marrow", serialize = "FleshAndMarrow")]
    FleshAndMarrow,
    /// Dry pantry staples (Grains, Pasta, Legumes).
    #[strum(
        serialize = "Shelf Stable Carbohydrates",
        serialize = "ShelfStableCarbohydrates"
    )]
    ShelfStableCarbohydrates,
    /// Yeast-risen or unleavened grain products (Bakery).
    #[strum(serialize = "Cultured Doughs", serialize = "CulturedDoughs")]
    CulturedDoughs,
    /// Non-alcoholic hydration solutions (Water, Tea, Coffee).
    #[strum(serialize = "Liquefied Hydration", serialize = "LiquefiedHydration")]
    LiquefiedHydration,
    /// Flavor enhancers and cooking mediums (Oils, Spices, Sauces).
    #[strum(
        serialize = "Condiments And Catalysts",
        serialize = "CondimentsAndCatalysts"
    )]
    CondimentsAndCatalysts,
    /// High-caloric, low-utility snacks (Sweets, Chips, "Junk Food").
    #[strum(
        serialize = "Nutrient Sparse Commodities",
        serialize = "NutrientSparseCommodities"
    )]
    NutrientSparseCommodities,
    /// Fermented or distilled recreational fluids (Alcohol).
    #[strum(serialize = "Ethanol Solutions", serialize = "EthanolSolutions")]
    EthanolSolutions,
    /// Supplements and medicinal agents (Health, First Aid).
    #[strum(
        serialize = "Biomedical Maintenance",
        serialize = "BiomedicalMaintenance"
    )]
    BiomedicalMaintenance,
    /// Non-consumable environmental maintenance (Cleaning, Toiletries).
    #[strum(
        serialize = "Sanitization And Utility",
        serialize = "SanitizationAndUtility"
    )]
    SanitizationAndUtility,
    /// Items failing systematic classification (Miscellaneous).
    #[strum(serialize = "Anomalous Inputs", serialize = "AnomalousInputs")]
    AnomalousInputs,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    mod grocery_category {
        use super::*;

        mod display {
            use super::*;

            #[test]
            fn returns_spaced_string_when_formatted() {
                assert_eq!(GroceryCategory::PrimaryFlora.to_string(), "Primary Flora");
                assert_eq!(
                    GroceryCategory::AnimalSecretions.to_string(),
                    "Animal Secretions"
                );
            }
        }

        mod from_str {
            use super::*;

            mod with_valid_input {
                use super::*;

                #[test]
                fn parses_successfully_when_string_contains_spaces() {
                    let cat = GroceryCategory::from_str("Primary Flora").unwrap();
                    assert_eq!(cat, GroceryCategory::PrimaryFlora);
                }

                #[test]
                fn parses_successfully_when_string_is_pascal_case() {
                    let cat = GroceryCategory::from_str("PrimaryFlora").unwrap();
                    assert_eq!(cat, GroceryCategory::PrimaryFlora);
                }

                #[test]
                fn parses_successfully_when_input_is_lowercase() {
                    let cat = GroceryCategory::from_str("primary flora").unwrap();
                    assert_eq!(cat, GroceryCategory::PrimaryFlora);
                }
            }

            mod with_invalid_input {
                use super::*;

                #[test]
                fn returns_error_when_category_is_unauthorized() {
                    let result = GroceryCategory::from_str("Junk Food");
                    assert!(result.is_err());
                }
            }
        }
    }
}
