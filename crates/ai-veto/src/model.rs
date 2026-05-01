use serde::Deserialize;

/// Structured response from the LLM.
#[derive(Debug, Deserialize)]
pub struct LlmResponse {
    pub is_approved: bool,
    pub category_assignment: String,
    pub moral_justification: String,
    pub resolved_item_key: String,
    pub suggested_display_name: String,
    pub resolved_unit: String,
    /// Optional context-specific multiplier for 'packs' or other discrete units
    /// (ADR 008)
    pub custom_multiplier: Option<String>,
}
