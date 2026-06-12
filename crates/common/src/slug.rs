//! Clinical slug conventions for canonical string representations.
//!
//! Slugs are lowercase, underscore-separated identifiers derived from
//! arbitrary input strings. Used for item keys and category labels to
//! ensure a consistent, sanitized form throughout the ledger.

use crate::taxonomy::GroceryCategory;

/// Converts an arbitrary string into a canonical slug for clinical telemetry.
///
/// Slugs are lowercase, use underscores instead of whitespace, and strip
/// non-alphanumeric characters (except underscores). Consecutive separators
/// are collapsed. Leading/trailing separators are trimmed.
pub fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut prev_was_sep = true;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_was_sep = false;
        } else if !prev_was_sep {
            slug.push('_');
            prev_was_sep = true;
        }
    }

    if slug.ends_with('_') {
        slug.truncate(slug.len() - 1);
    }

    slug
}

impl GroceryCategory {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::PrimaryFlora => "primary_flora",
            Self::AnimalSecretions => "animal_secretions",
            Self::FleshAndMarrow => "flesh_and_marrow",
            Self::ShelfStableCarbohydrates => "shelf_stable_carbohydrates",
            Self::CulturedDoughs => "cultured_doughs",
            Self::LiquefiedHydration => "liquefied_hydration",
            Self::CondimentsAndCatalysts => "condiments_and_catalysts",
            Self::NutrientSparseCommodities => "nutrient_sparse_commodities",
            Self::EthanolSolutions => "ethanol_solutions",
            Self::BiomedicalMaintenance => "biomedical_maintenance",
            Self::SanitizationAndUtility => "sanitization_and_utility",
            Self::AnomalousInputs => "anomalous_inputs",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod slugify {
        use super::*;

        mod when_input_is_already_clean {
            use super::*;

            #[test]
            fn returns_lowercased_version() {
                assert_eq!(slugify("Hello"), "hello");
            }

            #[test]
            fn preserves_snake_case() {
                assert_eq!(slugify("hello_world"), "hello_world");
            }

            #[test]
            fn preserves_digits() {
                assert_eq!(slugify("milk2"), "milk2");
            }
        }

        mod when_input_contains_whitespace {
            use super::*;

            #[test]
            fn replaces_spaces_with_underscores() {
                assert_eq!(slugify("Milk Whole"), "milk_whole");
            }

            #[test]
            fn replaces_tabs_with_underscores() {
                assert_eq!(slugify("Milk\tWhole"), "milk_whole");
            }

            #[test]
            fn collapses_consecutive_spaces() {
                assert_eq!(slugify("Milk  Whole"), "milk_whole");
            }
        }

        mod when_input_contains_special_characters {
            use super::*;

            #[test]
            fn strips_hyphens() {
                assert_eq!(slugify("Milk-Whole"), "milk_whole");
            }

            #[test]
            fn strips_periods() {
                assert_eq!(slugify("milk.whole"), "milk_whole");
            }

            #[test]
            fn strips_leading_special_chars() {
                assert_eq!(slugify("__milk"), "milk");
            }

            #[test]
            fn strips_trailing_special_chars() {
                assert_eq!(slugify("milk__"), "milk");
            }

            #[test]
            fn collapses_mixed_special_chars() {
                assert_eq!(slugify("milk_-_whole"), "milk_whole");
            }
        }

        mod when_input_is_empty_or_edge_case {
            use super::*;

            #[test]
            fn returns_empty_string_for_empty_input() {
                assert_eq!(slugify(""), "");
            }

            #[test]
            fn returns_empty_string_for_only_special_chars() {
                assert_eq!(slugify("!@#$%"), "");
            }
        }
    }

    mod grocery_category_slug {
        use super::*;

        mod returns_canonical_slug {
            use super::*;

            #[test]
            fn for_primary_flora() {
                assert_eq!(GroceryCategory::PrimaryFlora.slug(), "primary_flora");
            }

            #[test]
            fn for_animal_secretions() {
                assert_eq!(
                    GroceryCategory::AnimalSecretions.slug(),
                    "animal_secretions"
                );
            }

            #[test]
            fn for_flesh_and_marrow() {
                assert_eq!(GroceryCategory::FleshAndMarrow.slug(), "flesh_and_marrow");
            }

            #[test]
            fn for_shelf_stable_carbohydrates() {
                assert_eq!(
                    GroceryCategory::ShelfStableCarbohydrates.slug(),
                    "shelf_stable_carbohydrates"
                );
            }

            #[test]
            fn for_cultured_doughs() {
                assert_eq!(GroceryCategory::CulturedDoughs.slug(), "cultured_doughs");
            }

            #[test]
            fn for_liquefied_hydration() {
                assert_eq!(
                    GroceryCategory::LiquefiedHydration.slug(),
                    "liquefied_hydration"
                );
            }

            #[test]
            fn for_condiments_and_catalysts() {
                assert_eq!(
                    GroceryCategory::CondimentsAndCatalysts.slug(),
                    "condiments_and_catalysts"
                );
            }

            #[test]
            fn for_nutrient_sparse_commodities() {
                assert_eq!(
                    GroceryCategory::NutrientSparseCommodities.slug(),
                    "nutrient_sparse_commodities"
                );
            }

            #[test]
            fn for_ethanol_solutions() {
                assert_eq!(
                    GroceryCategory::EthanolSolutions.slug(),
                    "ethanol_solutions"
                );
            }

            #[test]
            fn for_biomedical_maintenance() {
                assert_eq!(
                    GroceryCategory::BiomedicalMaintenance.slug(),
                    "biomedical_maintenance"
                );
            }

            #[test]
            fn for_sanitization_and_utility() {
                assert_eq!(
                    GroceryCategory::SanitizationAndUtility.slug(),
                    "sanitization_and_utility"
                );
            }

            #[test]
            fn for_anomalous_inputs() {
                assert_eq!(GroceryCategory::AnomalousInputs.slug(), "anomalous_inputs");
            }
        }
    }
}
