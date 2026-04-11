use anyhow::Result;
use async_trait::async_trait;
use common::proto::v1::GroceryItem;
use common::proto::v1::MutationIntent;

/// Result of an AI Veto evaluation.
#[derive(Debug, Clone)]
pub struct VetoOutcome {
    pub is_approved: bool,
    pub category_assignment: String,
    pub moral_justification: String,
}

/// Internal bridge for communicating with the AI Veto Node.
///
/// This trait decouples the Raft Leader from the specific gRPC
/// implementation of the Policy Service.
#[async_trait]
pub trait VetoRelay: Send + Sync {
    /// Evaluates a proposed mutation against the current inventory and "moral"
    /// heuristics.
    async fn evaluate(
        &self,
        intent: &MutationIntent,
        current_inventory: &[GroceryItem],
    ) -> Result<VetoOutcome>;
}

/// A skeletal mock implementation that approves everything.
/// Used for Phase 2 and 3 infrastructure verification.
pub struct MockVetoRelay;

#[async_trait]
impl VetoRelay for MockVetoRelay {
    async fn evaluate(
        &self,
        _intent: &MutationIntent,
        _current_inventory: &[GroceryItem],
    ) -> Result<VetoOutcome> {
        Ok(VetoOutcome {
            is_approved: true,
            category_assignment: "Anomalous Inputs".to_string(), // Default fallback
            moral_justification: "Skeletal approval for infrastructure verification.".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod mock_relay {
        use super::*;

        #[tokio::test]
        async fn approves_all_proposals() -> Result<()> {
            let relay = MockVetoRelay;
            let intent = MutationIntent::default();
            let outcome = relay.evaluate(&intent, &[]).await?;

            assert!(outcome.is_approved);
            assert!(outcome.moral_justification.contains("Skeletal"));
            Ok(())
        }
    }
}
