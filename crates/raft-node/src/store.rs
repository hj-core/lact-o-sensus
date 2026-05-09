use std::collections::HashMap;

use async_trait::async_trait;
use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationStatus;
use common::types::LogIndex;
use gateway::ingress::InventorySource;
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
}

#[async_trait]
impl InventorySource for LactoStore {
    async fn get_inventory(&self) -> Vec<GroceryItem> {
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

        // UNIFIED LEDGER: We only update inventory for successful mutations.
        // Rejections and Vetoes only impact the Session Table (implemented in Step 3).
        if mutation.status != MutationStatus::Committed as i32 {
            info!(
                "FSM[{}]: Recording completion of sequence {} with status {:?}",
                index,
                mutation.sequence_id,
                MutationStatus::try_from(mutation.status).unwrap_or(MutationStatus::Unspecified)
            );
            return Ok(());
        }

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

#[cfg(test)]
mod tests {
    use common::proto::v1::app::MutationStatus;
    use common::types::ClientId;
    use common::types::SequenceId;

    use super::*;

    mod apply {
        use super::*;

        fn mock_mutation(status: MutationStatus) -> CommittedMutation {
            CommittedMutation::new(
                &ClientId::generate(),
                SequenceId::new(1),
                "milk".to_string(),
                "Milk".to_string(),
                "1000".to_string(),
                "ml".to_string(),
                "ml".to_string(),
                "Dairy".to_string(),
                "add milk".to_string(),
                "Approved".to_string(),
                false,
                status,
                std::time::SystemTime::now(),
            )
        }

        #[tokio::test]
        async fn updates_inventory_when_status_is_committed() {
            let store = LactoStore::new();
            let mutation = mock_mutation(MutationStatus::Committed);
            let mut data = Vec::new();
            mutation.encode(&mut data).unwrap();

            store.apply(LogIndex::new(1), &data).await.unwrap();

            let inventory = store.get_inventory().await;
            assert_eq!(inventory.len(), 1);
            assert_eq!(inventory[0].item_key, "milk");
            assert_eq!(inventory[0].quantity, "1000");
        }

        #[tokio::test]
        async fn does_not_update_inventory_when_status_is_vetoed() {
            let store = LactoStore::new();
            let mutation = mock_mutation(MutationStatus::Vetoed);
            let mut data = Vec::new();
            mutation.encode(&mut data).unwrap();

            // apply Vetoed mutation
            store.apply(LogIndex::new(1), &data).await.unwrap();

            // Verify inventory is still empty
            let inventory = store.get_inventory().await;
            assert!(inventory.is_empty());
        }

        #[tokio::test]
        async fn deletes_item_when_is_delete_is_true() {
            let store = LactoStore::new();

            // 1. Add item
            let add_mut = mock_mutation(MutationStatus::Committed);
            let mut add_data = Vec::new();
            add_mut.encode(&mut add_data).unwrap();
            store.apply(LogIndex::new(1), &add_data).await.unwrap();

            // 2. Delete item
            let mut del_mut = mock_mutation(MutationStatus::Committed);
            del_mut.is_delete = true;
            let mut del_data = Vec::new();
            del_mut.encode(&mut del_data).unwrap();
            store.apply(LogIndex::new(2), &del_data).await.unwrap();

            // 3. Verify
            let inventory = store.get_inventory().await;
            assert!(inventory.is_empty());
        }
    }
}
