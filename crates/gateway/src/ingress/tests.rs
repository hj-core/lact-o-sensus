use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use common::app_api::InventoryReader;
use common::app_api::SessionProvider;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStatus;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::types::errors::ConsensusError;
use common::types::trace::TraceId;
use raft_engine::ConsensusAuthority;
use raft_engine::ConsensusHandle;

use crate::ingress::IngressConfig;
use crate::ingress::IngressDispatcher;
use crate::ingress::test_utils::*;

mod propose_mutation {
    use common::proto::v1::app::CommittedMutation;
    use common::proto::v1::app::OperationType;
    use common::proto::v1::app::SessionRecord;
    use common::proto::v1::app::ingress_service_server::IngressService;
    use common::types::errors::FsmError;
    use prost::Message;

    use super::*;
    use crate::veto::VetoError;
    use crate::veto::VetoOutcome;
    use crate::veto::VetoRelay;

    // --- Phase 0: Leadership Authority ---

    #[tokio::test]
    async fn returns_rejected_when_not_leader() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: false,
            leader_hint: "http://leader:50051".to_string(),
            rejection_reason: "Node is a Follower".to_string(),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: 1,
            intent: None,
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Rejected as i32);
        assert_eq!(response.leader_hint, "http://leader:50051");
        assert!(response.error_message.contains("Follower"));
    }

    #[tokio::test]
    async fn returns_error_when_node_is_poisoned() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            is_poisoned: true,
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: 1,
            intent: None,
        });

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Internal);
        assert!(status.message().contains("Node is poisoned"));
    }

    // --- Phase 1: Deduplication (EOS) ---

    #[tokio::test]
    async fn returns_cached_success_on_duplicate_sequence() {
        #[derive(Debug)]
        struct DuplicateRaft {
            mock: Arc<MockRaftHandle>,
        }
        #[async_trait]
        impl ConsensusHandle for DuplicateRaft {
            async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                self.mock.propose(data).await
            }

            async fn await_commit(&self, index: LogIndex) -> Result<(), ConsensusError> {
                self.mock.await_commit(index).await
            }

            async fn await_apply(&self, index: LogIndex) -> Result<(), ConsensusError> {
                self.mock.await_apply(index).await
            }

            fn authority(&self) -> ConsensusAuthority {
                self.mock.authority()
            }

            async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                self.mock.verify_leadership().await
            }
        }

        #[derive(Debug)]
        struct DuplicateSource {
            committed_index: LogIndex,
        }
        impl SessionProvider for DuplicateSource {
            fn check_session(
                &self,
                cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(Some(SessionRecord::new(
                    cid,
                    SequenceId::new(1),
                    MutationStatus::Committed,
                    self.committed_index,
                    String::new(),
                    prost_types::Timestamp::default(),
                )))
            }
        }
        impl InventoryReader for DuplicateSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let committed_index = LogIndex::new(42);
        let raft = Arc::new(DuplicateRaft {
            mock: successful_raft(),
        });
        let inventory = Arc::new(DuplicateSource { committed_index });
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

        assert_eq!(response.status, MutationStatus::Committed as i32);
        assert_eq!(response.state_version, committed_index.as_u64());
    }

    // --- Phase 2: Concurrency & Syntactic (Layer 2) ---

    #[tokio::test]
    async fn rejects_mutation_violating_dimensional_fence() {
        let existing_item = GroceryItem::new(
            "milk_whole".to_string(),
            "1000".to_string(),
            "g".to_string(), // Stored as MASS
            "Animal Secretions".to_string(),
            "client".to_string(),
            prost_types::Timestamp::default(),
            LogIndex::new(1),
            "g".to_string(),
        );

        let raft = successful_raft();
        let inventory = Arc::new(MockInventorySource {
            items: vec![existing_item],
            ..Default::default()
        });

        // AI resolves the same item but with VOLUME units (ml)
        let veto = Arc::new(MockVetoRelay {
            outcome: Some(VetoOutcome {
                is_approved: true,
                resolved_item_key: "milk_whole".to_string(),
                resolved_unit: "ml".to_string(), // VOLUME
                ..valid_outcome()
            }),
            ..Default::default()
        });

        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, veto);
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("500".to_string()),
                Some("ml".to_string()),
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(
            response
                .error_message
                .contains("A physical unit mismatch was detected")
        );
    }

    #[tokio::test]
    async fn normalizes_intent_syntactically() {
        let raft = successful_raft();
        let dispatcher = mock_dispatcher(
            raft.clone(),
            successful_inventory(),
            successful_inventory(),
            Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: true,
                    category_assignment: "Primary Flora".to_string(),
                    moral_justification: "Mock justification".to_string(),
                    resolved_item_key: "bananas".to_string(),
                    suggested_display_name: "Bananas".to_string(),
                    resolved_unit: "units".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            }),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "  BANANAS  ".to_string(),
                Some(" 5 ".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let _ = dispatcher.propose_mutation(req).await.unwrap();

        let proposals = raft.proposals.lock().unwrap();
        assert_eq!(proposals.len(), 1);
        let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
        assert_eq!(mutation.resolved_item_key, "bananas");
        assert_eq!(mutation.updated_base_quantity, "5");
    }

    #[tokio::test]
    async fn verifies_full_consensus_serialization() {
        let raft = successful_raft();
        let dispatcher = mock_dispatcher(
            raft.clone(),
            successful_inventory(),
            successful_inventory(),
            Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: true,
                    category_assignment: "Animal Secretions".to_string(),
                    moral_justification: "Milk is ethical".to_string(),
                    resolved_item_key: "milk_whole".to_string(),
                    suggested_display_name: "Whole Milk".to_string(),
                    resolved_unit: "gal".to_string(),
                    conversion_multiplier_to_base: "3785.4118".to_string(),
                }),
                ..Default::default()
            }),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "  MiLk  ".to_string(),
                Some(" 1.5 ".to_string()),
                Some(" gal ".to_string()),
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

        assert_eq!(response.status, MutationStatus::Committed as i32);

        let proposals = raft.proposals.lock().unwrap();
        assert_eq!(proposals.len(), 1);
        let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();

        // Verification of SI Stabilization (1.5 * 3785.4118 = 5678.1177)
        assert_eq!(mutation.resolved_item_key, "milk_whole");
        assert_eq!(mutation.updated_base_quantity, "5678.1177");
        assert_eq!(mutation.base_unit, "ml");
        assert_eq!(mutation.display_unit, "gal");

        // Verification of RAW Audit Log (Must preserve original messy input)
        assert_eq!(mutation.raw_user_input, "Add 1.5 gal MiLk");
    }

    #[tokio::test]
    async fn rejects_missing_quantity_for_add_operation() {
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
                successful_veto(),
            )
        };
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new("bananas".to_string(), None, None, None, OperationType::Add),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn successfully_handles_delete_operation() {
        let raft = successful_raft();
        let dispatcher = mock_dispatcher(
            raft.clone(),
            successful_inventory(),
            successful_inventory(),
            Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: true,
                    category_assignment: "Animal Secretions".to_string(),
                    moral_justification: "Item removed".to_string(),
                    resolved_item_key: "milk_whole".to_string(),
                    suggested_display_name: "Whole Milk".to_string(),
                    resolved_unit: "ml".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            }),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new("milk".to_string(), None, None, None, OperationType::Delete),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

        assert_eq!(response.status, MutationStatus::Committed as i32);

        let proposals = raft.proposals.lock().unwrap();
        let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
        assert!(mutation.is_delete);
        assert_eq!(mutation.resolved_item_key, "milk_whole");
    }

    #[tokio::test]
    async fn rejects_delete_operation_with_quantity() {
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
                successful_veto(),
            )
        };
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Delete,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status
                .message()
                .contains("DELETE operations must not contain a quantity string")
        );
    }

    #[tokio::test]
    async fn rejects_when_item_key_is_empty() {
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
                successful_veto(),
            )
        };
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "   ".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_when_category_hint_is_invalid() {
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
                successful_veto(),
            )
        };
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                Some("Forbidden Snacks".to_string()),
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("Invalid category hint"));
    }

    #[tokio::test]
    async fn rejects_negative_quantity_for_mutation_intents() {
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                successful_raft(),
                inventory.clone(),
                inventory,
                successful_veto(),
            )
        };
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("-5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("cannot be negative"));
    }

    #[tokio::test]
    async fn enforces_sequential_processing_via_lock() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use tokio::time::Duration;
        use tokio::time::sleep;

        #[derive(Debug)]
        struct SlowVetoRelay {
            active_calls: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl VetoRelay for SlowVetoRelay {
            async fn evaluate(
                &self,
                _client_id: ClientId,
                _intent: &MutationIntent,
                _current_inventory: &[GroceryItem],
                _timeout: Duration,
                _max_justification_len: usize,
                _trace_id: TraceId,
            ) -> Result<VetoOutcome, VetoError> {
                let current = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;

                let mut max = self.max_concurrent.load(Ordering::SeqCst);
                while current > max {
                    match self.max_concurrent.compare_exchange_weak(
                        max,
                        current,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(actual) => max = actual,
                    }
                }

                sleep(Duration::from_millis(100)).await;
                self.active_calls.fetch_sub(1, Ordering::SeqCst);

                Ok(valid_outcome())
            }
        }

        let raft = successful_raft();
        let active_calls = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let veto = Arc::new(SlowVetoRelay {
            active_calls: active_calls.clone(),
            max_concurrent: max_concurrent.clone(),
        });

        let dispatcher = Arc::new({
            let inventory = successful_inventory();
            mock_dispatcher(raft, inventory.clone(), inventory, veto)
        });

        let d1 = dispatcher.clone();
        let h1 = tokio::spawn(async move {
            let cid = ClientId::generate();
            let req = make_request(ProposeMutationRequest::new(
                &cid,
                SequenceId::new(1),
                MutationIntent::new(
                    "item1".to_string(),
                    Some("1".to_string()),
                    None,
                    None,
                    OperationType::Add,
                ),
            ));
            d1.propose_mutation(req).await
        });

        let d2 = dispatcher.clone();
        let h2 = tokio::spawn(async move {
            let cid = ClientId::generate();
            let req = make_request(ProposeMutationRequest::new(
                &cid,
                SequenceId::new(2),
                MutationIntent::new(
                    "item2".to_string(),
                    Some("2".to_string()),
                    None,
                    None,
                    OperationType::Add,
                ),
            ));
            d2.propose_mutation(req).await
        });

        let _ = tokio::try_join!(h1, h2).unwrap();

        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "Mutations were processed concurrently!"
        );
    }

    // --- Phase 3: Semantic AI Resolution (Layer 3) ---

    #[tokio::test]
    async fn returns_vetoed_when_ai_rejects() {
        let veto = Arc::new(MockVetoRelay {
            outcome: Some(VetoOutcome {
                is_approved: false,
                category_assignment: "Primary Flora".to_string(),
                moral_justification: "Mock justification".to_string(),
                resolved_item_key: "milk".to_string(),
                suggested_display_name: "Milk".to_string(),
                resolved_unit: "ml".to_string(),
                conversion_multiplier_to_base: "1".to_string(),
            }),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(successful_raft(), inventory.clone(), inventory, veto);
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(response.error_message.contains("Mock justification"));
    }

    #[tokio::test]
    async fn proposes_to_consensus_even_when_ai_rejects() {
        let raft = successful_raft();
        let veto = Arc::new(MockVetoRelay {
            outcome: Some(VetoOutcome {
                is_approved: false,
                moral_justification: "Rejected by AI".to_string(),
                ..valid_outcome()
            }),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft.clone(), inventory.clone(), inventory, veto);

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

        // 1. Verify client receives Vetoed status
        assert_eq!(response.status, MutationStatus::Vetoed as i32);

        // 2. Verify consensus was still reached (Unified Ledger mandate)
        let proposals = raft.proposals.lock().unwrap();
        assert_eq!(
            proposals.len(),
            1,
            "Vetoed mutation was NOT proposed to consensus!"
        );

        let mutation = CommittedMutation::decode(&proposals[0][..]).unwrap();
        assert_eq!(mutation.status, MutationStatus::Vetoed as i32);
        assert_eq!(mutation.moral_justification, "Rejected by AI");
    }

    #[tokio::test]
    async fn returns_error_on_ai_timeout() {
        let veto = Arc::new(MockVetoRelay {
            error: Some(VetoError::Timeout(Duration::from_secs(1))),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(successful_raft(), inventory.clone(), inventory, veto);
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
    }

    // --- Phase 3.5: Leader Demotion During AI Veto ---

    mod demotion_during_veto {
        use super::*;
        use crate::ingress::test_utils::DemotingRaftHandle;

        #[tokio::test]
        async fn redirects_when_demoted_during_mutation_processing() {
            let new_leader_hint = "http://new-leader:50052".to_string();
            let raft = Arc::new(DemotingRaftHandle::new(new_leader_hint.clone()));
            let veto = Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: false,
                    category_assignment: "Primary Flora".to_string(),
                    moral_justification: "Mock justification".to_string(),
                    resolved_item_key: "milk".to_string(),
                    suggested_display_name: "Milk".to_string(),
                    resolved_unit: "ml".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            });
            let inventory = successful_inventory();
            let dispatcher = mock_dispatcher(raft.clone(), inventory.clone(), inventory, veto);
            let cid = ClientId::generate();
            let req = make_request(ProposeMutationRequest::new(
                &cid,
                SequenceId::new(1),
                MutationIntent::new(
                    "bananas".to_string(),
                    Some("5".to_string()),
                    None,
                    None,
                    OperationType::Add,
                ),
            ));

            let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

            // 1. Must return Rejected status, not a gRPC error
            assert_eq!(response.status, MutationStatus::Rejected as i32);

            // 2. Must include new leader's address in the hint
            assert_eq!(response.leader_hint, new_leader_hint);

            // 3. Must NOT have proposed anything to Raft (demoted before propose)
            let proposals = raft.mock.proposals.lock().unwrap();
            assert!(
                proposals.is_empty(),
                "No mutation should be proposed when leader is demoted"
            );
        }
    }

    // --- Phase 4 & 5: Consensus & State Machine (Layer 4/5) ---

    #[tokio::test]
    async fn returns_vetoed_when_ai_hallucinates_metadata() {
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(
            successful_raft(),
            inventory.clone(),
            inventory,
            Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: true,
                    category_assignment: "Space Matter".to_string(), // Hallucination
                    moral_justification: "Approved".to_string(),
                    resolved_item_key: "milk".to_string(),
                    suggested_display_name: "Milk".to_string(),
                    resolved_unit: "g".to_string(),
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            }),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(
            response
                .error_message
                .contains("A physical unit mismatch was detected")
        );
    }

    #[tokio::test]
    async fn returns_vetoed_when_ai_provides_invalid_conversion() {
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(
            successful_raft(),
            inventory.clone(),
            inventory,
            Arc::new(MockVetoRelay {
                outcome: Some(VetoOutcome {
                    is_approved: true,
                    category_assignment: "Primary Flora".to_string(),
                    moral_justification: "Approved".to_string(),
                    resolved_item_key: "milk".to_string(),
                    suggested_display_name: "Milk".to_string(),
                    resolved_unit: "blorgs".to_string(), // Hallucination
                    conversion_multiplier_to_base: "1".to_string(),
                }),
                ..Default::default()
            }),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(
            response
                .error_message
                .contains("A physical unit mismatch was detected")
        );
    }

    #[tokio::test]
    async fn returns_error_on_consensus_failure() {
        #[derive(Debug, Default)]
        struct FailingRaft;
        #[async_trait]
        impl ConsensusHandle for FailingRaft {
            async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                Err(ConsensusError::Internal("Consensus failure".to_string()))
            }

            async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                Ok(())
            }

            async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                Ok(())
            }

            fn authority(&self) -> ConsensusAuthority {
                ConsensusAuthority {
                    is_leader: true,
                    is_poisoned: false,
                    ..Default::default()
                }
            }

            async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                Ok(())
            }
        }

        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(
            Arc::new(FailingRaft),
            inventory.clone(),
            inventory,
            successful_veto(),
        );
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn returns_error_on_consensus_timeout() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            commit_delay: Some(Duration::from_millis(50)),
            ..Default::default()
        });
        let inventory = successful_inventory();
        // Configure very short consensus timeout
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            successful_veto(),
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_millis(10), // Short consensus timeout
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "bananas".to_string(),
                Some("5".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
        assert!(err.message().contains("timed out"));
    }

    #[tokio::test]
    async fn retries_on_transient_ai_failure_and_succeeds() {
        let raft = successful_raft();
        let veto = Arc::new(FlakyVetoRelay {
            max_fails: 1,
            ..Default::default()
        });
        let inventory = successful_inventory();
        // Configured for 1 retry (max 2 attempts)
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            veto,
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_secs(1),
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Committed as i32);
    }

    #[tokio::test]
    async fn retries_on_ai_hallucination_and_succeeds() {
        let raft = successful_raft();
        let hallucination = VetoOutcome {
            is_approved: true,
            category_assignment: "Space Matter".to_string(), // Hallucination
            moral_justification: "Oops".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "ml".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };
        let success = VetoOutcome {
            is_approved: true,
            category_assignment: "Animal Secretions".to_string(),
            moral_justification: "Corrected".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "ml".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };
        let veto = Arc::new(HallucinatingVetoRelay {
            hallucination_outcome: hallucination,
            success_outcome: success,
            call_count: Mutex::new(0),
        });
        let inventory = successful_inventory();
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            veto,
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_secs(1),
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Committed as i32);
    }

    #[tokio::test]
    async fn vetoes_after_max_retries_exhausted_on_hallucination() {
        let raft = successful_raft();
        let hallucination = VetoOutcome {
            is_approved: true,
            category_assignment: "Space Matter".to_string(),
            moral_justification: "Still Hallucinating".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "ml".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };
        let veto = Arc::new(MockVetoRelay {
            outcome: Some(hallucination),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            veto,
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_secs(1),
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(
            response
                .error_message
                .contains("A physical unit mismatch was detected")
        );
    }

    #[tokio::test]
    async fn vetoes_after_max_retries_exhausted_on_mixed_failures() {
        let raft = successful_raft();
        let hallucination = VetoOutcome {
            is_approved: true,
            category_assignment: "Space Matter".to_string(), // Hallucination
            moral_justification: "Oops".to_string(),
            resolved_item_key: "milk".to_string(),
            suggested_display_name: "Milk".to_string(),
            resolved_unit: "ml".to_string(),
            conversion_multiplier_to_base: "1".to_string(),
        };
        let veto = Arc::new(MixedFailureVetoRelay {
            hallucination_outcome: hallucination,
            call_count: Mutex::new(0),
        });
        let inventory = successful_inventory();
        // Configured for 1 retry (2 attempts total)
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            veto,
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_secs(1),
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();

        // Should be VETOED because:
        // Attempt 1: Timeout (Consumed 1st attempt)
        // Attempt 2: Hallucination (Consumed 1 retry quota)
        // Quota exhausted -> definitive Veto
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert!(
            response
                .error_message
                .contains("A physical unit mismatch was detected")
        );
    }

    #[tokio::test]
    async fn does_not_retry_on_definitive_ai_veto() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        #[derive(Debug)]
        struct CountingVetoRelay {
            call_count: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl VetoRelay for CountingVetoRelay {
            async fn evaluate(
                &self,
                _client_id: ClientId,
                _intent: &MutationIntent,
                _current_inventory: &[GroceryItem],
                _timeout: Duration,
                _max_justification_len: usize,
                _trace_id: TraceId,
            ) -> Result<VetoOutcome, VetoError> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                Ok(VetoOutcome {
                    is_approved: false,
                    moral_justification: "Definitive NO".to_string(),
                    ..valid_outcome()
                })
            }
        }

        let raft = successful_raft();
        let call_count = Arc::new(AtomicUsize::new(0));
        let veto = Arc::new(CountingVetoRelay {
            call_count: call_count.clone(),
        });
        let inventory = successful_inventory();
        let dispatcher = IngressDispatcher::new(
            raft.clone(),
            inventory.clone(),
            inventory,
            veto,
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_secs(1),
                veto_max_retries: 10,
                max_justification_len: 512,
            },
        );

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "unethical item".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let _ = dispatcher.propose_mutation(req).await.unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_when_ai_veto_violates_causal_integrity() {
        #[derive(Debug)]
        struct ByzantineVetoRelay;
        #[async_trait]
        impl VetoRelay for ByzantineVetoRelay {
            async fn evaluate(
                &self,
                _client_id: ClientId,
                _intent: &MutationIntent,
                _current_inventory: &[GroceryItem],
                _timeout: Duration,
                _max_justification_len: usize,
                _trace_id: TraceId,
            ) -> Result<VetoOutcome, VetoError> {
                // Simulate the bridge detecting a TraceId mismatch or missing ID
                Err(VetoError::CausalIntegrityViolation)
            }
        }

        let raft = successful_raft();
        let dispatcher = {
            let inventory = successful_inventory();
            mock_dispatcher(
                raft,
                inventory.clone(),
                inventory,
                Arc::new(ByzantineVetoRelay),
            )
        };

        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let result = dispatcher.propose_mutation(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("Causal Integrity Violation"));
    }
}

mod exactly_once_semantics {
    use common::proto::v1::app::OperationType;
    use common::proto::v1::app::SessionRecord;
    use common::proto::v1::app::ingress_service_server::IngressService;
    use common::types::errors::FsmError;

    use super::*;

    #[tokio::test]
    async fn returns_cached_outcome_when_sequence_is_duplicate() {
        let raft = successful_raft();
        let sid = SequenceId::new(42);
        let committed_index = LogIndex::new(100);

        #[derive(Debug, Default)]
        struct MockSource {
            record: Option<SessionRecord>,
        }
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                _cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(self.record.clone())
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let inventory = Arc::new(MockSource {
            record: Some(SessionRecord::new(
                &ClientId::generate(), // Dummy for duplicate check
                sid,
                MutationStatus::Committed,
                committed_index,
                "Approved".to_string(),
                prost_types::Timestamp::default(),
            )),
        });

        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: sid.as_u64(),
            intent: Some(MutationIntent::default()),
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Committed as i32);
        assert_eq!(response.state_version, committed_index.as_u64());
    }

    #[tokio::test]
    async fn replays_cached_veto_with_justification() {
        let raft = successful_raft();
        let sid = SequenceId::new(42);

        #[derive(Debug, Default)]
        struct MockSource {
            record: Option<SessionRecord>,
        }
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                _cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(self.record.clone())
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let inventory = Arc::new(MockSource {
            record: Some(SessionRecord::new(
                &ClientId::generate(),
                sid,
                MutationStatus::Vetoed,
                LogIndex::new(10),
                "Reason".to_string(),
                prost_types::Timestamp::default(),
            )),
        });

        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: sid.as_u64(),
            intent: Some(MutationIntent::default()),
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Vetoed as i32);
        assert_eq!(response.error_message, "Reason");
    }

    #[tokio::test]
    async fn rejects_request_when_sequence_is_stale() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource;
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(Some(SessionRecord::new(
                    cid,
                    SequenceId::new(10),
                    MutationStatus::Committed,
                    LogIndex::ZERO,
                    String::new(),
                    prost_types::Timestamp::default(),
                )))
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: 5, // Stale: cluster is at 10
            intent: Some(MutationIntent::default()),
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Rejected as i32);
        assert!(
            response
                .error_message
                .contains("Secure Clinical: Stale Sequence")
        );
        assert!(!response.error_message.contains("10")); // Opaque check
    }

    #[tokio::test]
    async fn rejects_request_when_sequence_gap_detected() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource;
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(Some(SessionRecord::new(
                    cid,
                    SequenceId::new(1),
                    MutationStatus::Committed,
                    LogIndex::ZERO,
                    String::new(),
                    prost_types::Timestamp::default(),
                )))
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: 3, // Gap: expecting 2
            intent: Some(MutationIntent::default()),
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Rejected as i32);
        assert!(
            response
                .error_message
                .contains("Secure Clinical: Sequence Continuity Violation")
        );
        assert!(!response.error_message.contains("2")); // Opaque check
    }

    #[tokio::test]
    async fn rejects_request_when_new_client_skips_start_sequence() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource; // Returns None for check_session
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                _cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(None)
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(5),
            MutationIntent::new(
                "milk".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Rejected as i32);
        assert!(
            response
                .error_message
                .contains("Secure Clinical: Session Initialization Violation")
        );
    }

    #[tokio::test]
    async fn accepts_request_when_new_client_starts_at_sequence_one() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource;
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                _cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(None) // New client
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(1),
            MutationIntent::new(
                "item".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Committed as i32);
    }

    #[tokio::test]
    async fn accepts_request_when_sequence_is_next_in_line() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource;
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(Some(SessionRecord::new(
                    cid,
                    SequenceId::new(1),
                    MutationStatus::Committed,
                    LogIndex::ZERO,
                    String::new(),
                    prost_types::Timestamp::default(),
                )))
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }

        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let cid = ClientId::generate();
        let req = make_request(ProposeMutationRequest::new(
            &cid,
            SequenceId::new(2),
            MutationIntent::new(
                "item".to_string(),
                Some("1".to_string()),
                None,
                None,
                OperationType::Add,
            ),
        ));

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Committed as i32);
    }

    #[tokio::test]
    async fn rejects_request_when_sequence_id_is_zero() {
        let raft = successful_raft();
        #[derive(Debug, Default)]
        struct MockSource;
        impl SessionProvider for MockSource {
            fn check_session(
                &self,
                _cid: &ClientId,
                _sid: SequenceId,
            ) -> Result<Option<SessionRecord>, FsmError> {
                Ok(None)
            }
        }
        impl InventoryReader for MockSource {
            fn get_inventory(&self) -> Vec<GroceryItem> {
                vec![]
            }

            fn current_version(&self) -> LogIndex {
                LogIndex::ZERO
            }
        }
        let mock_source = Arc::new(MockSource);
        let dispatcher = mock_dispatcher(raft, mock_source.clone(), mock_source, successful_veto());
        let req = make_request(ProposeMutationRequest {
            client_id: ClientId::generate().as_str().to_string(),
            sequence_id: 0, // Forbidden by protocol
            intent: Some(MutationIntent::default()),
        });

        let response = dispatcher.propose_mutation(req).await.unwrap().into_inner();
        assert_eq!(response.status, MutationStatus::Rejected as i32);
        assert!(
            response
                .error_message
                .contains("Secure Clinical: Protocol Violation")
        );
    }
}

mod query_state {
    use common::proto::v1::app::ingress_service_server::IngressService;

    use super::*;

    #[tokio::test]
    async fn returns_rejected_when_not_leader() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: false,
            leader_hint: "http://leader:50051".to_string(),
            rejection_reason: "Node is a Follower".to_string(),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(QueryStateRequest::new(None, None));

        let response = dispatcher.query_state(req).await.unwrap().into_inner();
        assert_eq!(response.status, QueryStatus::Rejected as i32);
        assert_eq!(response.leader_hint, "http://leader:50051");
    }

    #[tokio::test]
    async fn returns_error_when_node_is_poisoned() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            is_poisoned: true,
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());
        let req = make_request(QueryStateRequest::new(None, None));

        let result = dispatcher.query_state(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Internal);
        assert!(status.message().contains("Node is poisoned"));
    }

    #[tokio::test]
    async fn returns_all_items_when_no_filter_is_provided() {
        let items = vec![
            GroceryItem::new(
                "milk".to_string(),
                "1000".to_string(),
                "ml".to_string(),
                "Dairy".to_string(),
                "client".to_string(),
                prost_types::Timestamp::default(),
                LogIndex::new(0),
                "ml".to_string(),
            ),
            GroceryItem::new(
                "eggs".to_string(),
                "12".to_string(),
                "units".to_string(),
                "Dairy".to_string(),
                "client".to_string(),
                prost_types::Timestamp::default(),
                LogIndex::new(0),
                "units".to_string(),
            ),
        ];

        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            ..Default::default()
        });
        let inventory = Arc::new(MockInventorySource {
            items: items.clone(),
            ..Default::default()
        });
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

        let req = make_request(QueryStateRequest::new(None, None));

        let response = dispatcher.query_state(req).await.unwrap().into_inner();
        assert_eq!(response.status, QueryStatus::Success as i32);
        assert_eq!(response.items.len(), 2);

        let mut response_items = response.items;
        response_items.sort_by_key(|i| i.item_key.clone());
        assert_eq!(response_items[0].item_key, "eggs");
        assert_eq!(response_items[1].item_key, "milk");
    }

    #[tokio::test]
    async fn filters_items_by_substring_match() {
        let items = vec![
            GroceryItem::new(
                "milk_whole".to_string(),
                "1".to_string(),
                "unit".to_string(),
                "Dairy".to_string(),
                "client".to_string(),
                prost_types::Timestamp::default(),
                LogIndex::new(0),
                "unit".to_string(),
            ),
            GroceryItem::new(
                "milk-skim".to_string(),
                "1".to_string(),
                "unit".to_string(),
                "Dairy".to_string(),
                "client".to_string(),
                prost_types::Timestamp::default(),
                LogIndex::new(0),
                "unit".to_string(),
            ),
            GroceryItem::new(
                "eggs".to_string(),
                "12".to_string(),
                "units".to_string(),
                "Dairy".to_string(),
                "client".to_string(),
                prost_types::Timestamp::default(),
                LogIndex::new(0),
                "units".to_string(),
            ),
        ];

        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            ..Default::default()
        });
        let inventory = Arc::new(MockInventorySource {
            items: items.clone(),
            ..Default::default()
        });
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

        let req = make_request(QueryStateRequest::new(Some("milk".to_string()), None));

        let response = dispatcher.query_state(req).await.unwrap().into_inner();
        assert_eq!(response.status, QueryStatus::Success as i32);

        let mut response_items = response.items;
        response_items.sort_by_key(|i| i.item_key.clone());

        assert_eq!(response_items.len(), 2);
        assert_eq!(response_items[0].item_key, "milk-skim");
        assert_eq!(response_items[1].item_key, "milk_whole");
    }

    #[tokio::test]
    async fn rejects_query_when_await_apply_fails() {
        #[derive(Debug, Default)]
        struct FailingApplyRaft;
        #[async_trait]
        impl ConsensusHandle for FailingApplyRaft {
            async fn propose(&self, _data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
                Ok(LogIndex::new(1))
            }

            async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                Ok(())
            }

            async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
                Err(ConsensusError::Poisoned)
            }

            fn authority(&self) -> ConsensusAuthority {
                ConsensusAuthority {
                    is_leader: true,
                    is_poisoned: false,
                    last_committed: LogIndex::new(100),
                    ..Default::default()
                }
            }

            async fn verify_leadership(&self) -> Result<(), ConsensusError> {
                Ok(())
            }
        }

        let raft = Arc::new(FailingApplyRaft);
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

        let req = make_request(QueryStateRequest::new(None, Some(LogIndex::new(10))));

        let result = dispatcher.query_state(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Aborted);
        assert!(status.message().contains("fatal state"));
    }

    #[tokio::test]
    async fn rejects_query_when_await_apply_timeouts() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            last_committed: LogIndex::new(100),
            apply_delay: Some(Duration::from_millis(50)),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = IngressDispatcher::new(
            raft,
            inventory.clone(),
            inventory,
            successful_veto(),
            IngressConfig {
                veto_timeout: Duration::from_secs(1),
                consensus_timeout: Duration::from_millis(10), // Short consensus timeout
                veto_max_retries: 1,
                max_justification_len: 512,
            },
        );

        let req = make_request(QueryStateRequest::new(None, Some(LogIndex::new(10))));

        let result = dispatcher.query_state(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
        assert!(status.message().contains("timed out"));
    }

    #[tokio::test]
    async fn rejects_query_exceeding_horizon() {
        let raft = Arc::new(MockRaftHandle {
            is_leader: true,
            last_committed: LogIndex::new(5),
            ..Default::default()
        });
        let inventory = successful_inventory();
        let dispatcher = mock_dispatcher(raft, inventory.clone(), inventory, successful_veto());

        let req = make_request(QueryStateRequest::new(None, Some(LogIndex::new(10)))); // Exceeds horizon (5)

        let result = dispatcher.query_state(req).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("exceeds consistent horizon"));
    }
}
