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
