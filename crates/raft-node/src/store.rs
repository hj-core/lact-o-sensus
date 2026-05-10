use async_trait::async_trait;
use common::proto::v1::app::CommittedMutation;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationStatus;
use common::types::LogIndex;
use common::types::errors::FsmError;
use common::raft_api::InventorySource;
use prost::Message;
use sled::Transactional;
use sled::transaction::TransactionResult;
use tracing::info;

use common::raft_api::StateMachine;

/// Persistent implementation of the Lact-O-Sensus state machine using `sled`.
///
/// This store satisfies the StateMachine trait by deserializing
/// CommittedMutation bytes and updating a localized inventory persisted on
/// disk.
///
/// TREE ARCHITECTURE:
/// To ensure logical isolation within the FSM database:
/// 1. "inventory": Exclusively for [resolved_item_key (String) => GroceryItem
///    (Protobuf)]
/// 2. "meta": Exclusively for [Key (String) => Metadata (Binary)], e.g.,
///    last_applied_index.
#[derive(Debug)]
pub struct LactoStore {
    db: sled::Db,
    inventory: sled::Tree,
    meta: sled::Tree,
}

impl LactoStore {
    const KEY_LAST_APPLIED: &'static [u8] = b"last_applied";
    const TREE_INVENTORY: &'static str = "inventory";
    const TREE_META: &'static str = "meta";

    pub fn new(db: sled::Db) -> Result<Self, FsmError> {
        let inventory = db
            .open_tree(Self::TREE_INVENTORY)
            .map_err(|e| FsmError::Persistence(format!("Failed to open inventory tree: {}", e)))?;
        let meta = db
            .open_tree(Self::TREE_META)
            .map_err(|e| FsmError::Persistence(format!("Failed to open meta tree: {}", e)))?;

        Ok(Self {
            db,
            inventory,
            meta,
        })
    }

    /// Factory to construct a GroceryItem from a committed mutation record.
    fn item_from_mutation(index: LogIndex, mutation: CommittedMutation) -> GroceryItem {
        GroceryItem {
            item_key: mutation.resolved_item_key,
            quantity: mutation.updated_base_quantity,
            unit: mutation.base_unit,
            category: mutation.updated_category,
            last_modifier_id: mutation.client_id,
            last_activity: mutation.event_time,
            state_version: index.value(),
        }
    }
}

#[async_trait]
impl InventorySource for LactoStore {
    async fn get_inventory(&self) -> Vec<GroceryItem> {
        self.inventory
            .iter()
            .filter_map(|res| {
                res.ok()
                    .and_then(|(_, v)| GroceryItem::decode(v.as_ref()).ok())
            })
            .collect()
    }

    async fn current_version(&self) -> LogIndex {
        StateMachine::last_applied_index(self)
    }
}

#[async_trait]
impl StateMachine for LactoStore {
    fn last_applied_index(&self) -> LogIndex {
        self.meta
            .get(Self::KEY_LAST_APPLIED)
            .ok()
            .flatten()
            .map(|k| {
                let bytes: [u8; 8] = k.as_ref().try_into().unwrap_or([0; 8]);
                LogIndex::new(u64::from_be_bytes(bytes))
            })
            .unwrap_or(LogIndex::ZERO)
    }

    async fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), FsmError> {
        let mutation = CommittedMutation::decode(data).map_err(|e| {
            FsmError::Deserialization(format!(
                "Failed to deserialize mutation at index {}: {}",
                index, e
            ))
        })?;

        // UNIFIED LEDGER: We acknowledge all facts to advance the apply index,
        // but only update physical inventory if status is COMMITTED.
        if mutation.status != MutationStatus::Committed as i32 {
            info!(
                "FSM[{}]: Recording completion of sequence {} with status {:?}",
                index,
                mutation.sequence_id,
                MutationStatus::try_from(mutation.status).unwrap_or(MutationStatus::Unspecified)
            );

            // Even for Vetoes, we must update the last_applied index to ensure
            // replay logic (Step 3.2) correctly skips processed entries.
            self.meta
                .insert(Self::KEY_LAST_APPLIED, &index.value().to_be_bytes())
                .map_err(|e| {
                    FsmError::Persistence(format!("Failed to update last_applied: {}", e))
                })?;
            self.db
                .flush()
                .map_err(|e| FsmError::Persistence(format!("FSM flush failure: {}", e)))?;
            return Ok(());
        }

        // --- Physical Mutation & Index Update ---
        // Using a transaction to ensure that the inventory update and the
        // last_applied advancement are atomic.
        let inventory_tree = self.inventory.clone();
        let meta_tree = self.meta.clone();

        let res: TransactionResult<(), ()> =
            (&inventory_tree, &meta_tree).transaction(|(inventory, meta)| {
                if mutation.is_delete {
                    inventory.remove(mutation.resolved_item_key.as_bytes())?;
                } else {
                    let item = Self::item_from_mutation(index, mutation.clone());
                    inventory.insert(
                        mutation.resolved_item_key.as_bytes(),
                        item.encode_to_vec().as_slice(),
                    )?;
                }

                meta.insert(Self::KEY_LAST_APPLIED, &index.value().to_be_bytes())?;
                Ok(())
            });

        res.map_err(|e| FsmError::Persistence(format!("FSM transaction failed: {:?}", e)))?;

        info!(
            "FSM[{}]: Recording completion of sequence {} with status Committed",
            index, mutation.sequence_id
        );

        // Synchronous flush as mandated by ADR 001
        self.db.flush().map_err(|e| {
            FsmError::Persistence(format!("FSM persistence failure during flush: {}", e))
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::proto::v1::app::MutationStatus;
    use common::types::ClientId;
    use common::types::SequenceId;
    use tempfile::tempdir;

    use super::*;

    fn setup_store() -> LactoStore {
        let dir = tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        LactoStore::new(db).unwrap()
    }

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
            let store = setup_store();
            let mutation = mock_mutation(MutationStatus::Committed);
            let mut data = Vec::new();
            mutation.encode(&mut data).unwrap();

            store.apply(LogIndex::new(1), &data).await.unwrap();

            let inventory = store.get_inventory().await;
            assert_eq!(inventory.len(), 1);
            assert_eq!(inventory[0].item_key, "milk");
            assert_eq!(inventory[0].quantity, "1000");
            assert_eq!(store.last_applied_index(), LogIndex::new(1));
        }

        #[tokio::test]
        async fn does_not_update_inventory_when_status_is_vetoed() {
            let store = setup_store();
            let mutation = mock_mutation(MutationStatus::Vetoed);
            let mut data = Vec::new();
            mutation.encode(&mut data).unwrap();

            store.apply(LogIndex::new(1), &data).await.unwrap();

            let inventory = store.get_inventory().await;
            assert!(inventory.is_empty());
            assert_eq!(store.last_applied_index(), LogIndex::new(1));
        }

        #[tokio::test]
        async fn deletes_item_when_is_delete_is_true() {
            let store = setup_store();

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
            assert_eq!(store.last_applied_index(), LogIndex::new(2));
        }

        #[tokio::test]
        async fn survives_restart_with_consistent_state() {
            let dir = tempdir().unwrap();
            let db_path = dir.path();

            // 1. apply and shutdown
            {
                let db = sled::open(db_path).unwrap();
                let store = LactoStore::new(db).unwrap();
                let mut data = Vec::new();
                mock_mutation(MutationStatus::Committed)
                    .encode(&mut data)
                    .unwrap();

                store.apply(LogIndex::new(42), &data).await.unwrap();
            }

            // 2. Restart and verify
            {
                let db = sled::open(db_path).unwrap();
                let store = LactoStore::new(db).unwrap();

                let inventory = store.get_inventory().await;
                assert_eq!(inventory.len(), 1);
                assert_eq!(inventory[0].item_key, "milk");
                assert_eq!(store.last_applied_index(), LogIndex::new(42));
            }
        }
    }
}
