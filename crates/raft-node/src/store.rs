use std::collections::HashMap;

use async_trait::async_trait;
use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::GroceryItem;
use common::types::LogIndex;
use prost::Message;
use tokio::sync::RwLock;
use tonic::Status;
use tracing::info;

use crate::fsm::StateMachine;

/// In-memory implementation of the Lact-O-Sensus state machine.
///
/// This store satisfies the StateMachine trait by deserializing
/// CommittedMutation bytes and updating a localized inventory.
#[derive(Debug, Default)]
pub struct LactoStore {
    /// The canonical inventory of groceries.
    /// Key: resolved_item_key (Canonical Slug)
    inventory: RwLock<HashMap<String, GroceryItem>>,
}

impl LactoStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Provides a read-only snapshot of the current inventory.
    pub async fn get_inventory(&self) -> Vec<GroceryItem> {
        self.inventory.read().await.values().cloned().collect()
    }
}

#[async_trait]
impl StateMachine for LactoStore {
    async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Status> {
        let mutation = CommittedMutation::decode(data).map_err(|e| {
            Status::internal(format!(
                "Failed to deserialize mutation at index {}: {}",
                index, e
            ))
        })?;

        let mut inventory = self.inventory.write().await;

        if mutation.is_delete {
            info!(
                "FSM[{}]: Deleting item '{}'",
                index, mutation.resolved_item_key
            );
            inventory.remove(&mutation.resolved_item_key);
        } else {
            info!(
                "FSM[{}]: Upserting item '{}' (qty: {}, unit: {})",
                index,
                mutation.resolved_item_key,
                mutation.updated_base_quantity,
                mutation.base_unit
            );

            let item = GroceryItem {
                item_key: mutation.resolved_item_key.clone(),
                quantity: mutation.updated_base_quantity,
                unit: mutation.base_unit,
                category: mutation.updated_category,
                last_modifier_id: mutation.client_id,
                last_activity: mutation.event_time,
                state_version: index.value(),
            };

            inventory.insert(mutation.resolved_item_key, item);
        }

        Ok(())
    }
}
