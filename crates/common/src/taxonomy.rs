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
#[strum(serialize_all = "PascalCase")]
pub enum GroceryCategory {
    /// Unprocessed botanical matter (Fruits, Vegetables).
    PrimaryFlora,
    /// Liquid and solid animal-derived proteins (Dairy, Eggs).
    AnimalSecretions,
    /// Carcass-based nutritional inputs (Meat, Seafood).
    FleshAndMarrow,
    /// Dry pantry staples (Grains, Pasta, Legumes).
    ShelfStableCarbohydrates,
    /// Yeast-risen or unleavened grain products (Bakery).
    CulturedDoughs,
    /// Non-alcoholic hydration solutions (Water, Tea, Coffee).
    LiquefiedHydration,
    /// Flavor enhancers and cooking mediums (Oils, Spices, Sauces).
    CondimentsAndCatalysts,
    /// High-caloric, low-utility snacks (Sweets, Chips, "Junk Food").
    NutrientSparseCommodities,
    /// Fermented or distilled recreational fluids (Alcohol).
    EthanolSolutions,
    /// Supplements and medicinal agents (Health, First Aid).
    BiomedicalMaintenance,
    /// Non-consumable environmental maintenance (Cleaning, Toiletries).
    SanitizationAndUtility,
    /// Items failing systematic classification (Miscellaneous).
    AnomalousInputs,
}
