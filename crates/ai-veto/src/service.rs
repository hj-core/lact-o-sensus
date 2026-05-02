use std::str::FromStr;

use common::proto::v1::app::EvaluateProposalRequest;
use common::proto::v1::app::EvaluateProposalResponse;
use common::proto::v1::app::OperationType;
use common::proto::v1::app::policy_service_server::PolicyService;
use common::taxonomy::GroceryCategory;
use common::units::UnitRegistry;
use ollama_rs::Ollama;
use ollama_rs::generation::chat::ChatMessage;
use ollama_rs::generation::chat::MessageRole;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::parameters::FormatType;
use ollama_rs::generation::parameters::KeepAlive;
use ollama_rs::generation::parameters::ThinkType;
use ollama_rs::generation::parameters::TimeUnit;
use ollama_rs::models::ModelOptions;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::args::Args;
use crate::model::LlmResponse;

pub struct RealPolicyService {
    ollama: Ollama,
    args: Args,
}

impl RealPolicyService {
    pub fn new(args: Args) -> Self {
        info!(
            "Moral Advocate initializing. Model={}, Host={}:{}, Think={}, MaxTokens={}",
            args.model, args.ollama_host, args.ollama_port, args.think, args.max_tokens
        );
        Self {
            ollama: Ollama::new(args.ollama_host.clone(), args.ollama_port),
            args,
        }
    }

    /// Orchestrator: Constructs the isolated chat request for Ollama.
    /// Implements Prompt Caching by maintaining a static System role.
    fn build_chat_request(
        &self,
        req: &EvaluateProposalRequest,
    ) -> Result<ChatMessageRequest, Status> {
        let system_prompt = self.get_system_prompt();
        let user_prompt = self.build_user_prompt(req)?;

        let messages = vec![
            ChatMessage::new(MessageRole::System, system_prompt),
            ChatMessage::new(MessageRole::User, user_prompt),
        ];

        // Construct request with reasoning toggle and JSON enforcement
        let think_type = if self.args.think {
            ThinkType::True
        } else {
            ThinkType::False
        };

        // Cache-optimized options: lock context window to ensure prefix reuse.
        // Set hardware latency bound via num_predict.
        let options = ModelOptions::default()
            .num_ctx(4096)
            .num_predict(self.args.max_tokens as i32);

        let mut chat_req = ChatMessageRequest::new(self.args.model.clone(), messages)
            .think(think_type)
            .format(FormatType::Json)
            .options(options);

        // Keep model in memory to prevent Cold Start delays during leader election
        chat_req.keep_alive = Some(self.map_keep_alive());

        Ok(chat_req)
    }

    fn map_keep_alive(&self) -> KeepAlive {
        if self.args.keep_alive < 0 {
            KeepAlive::Indefinitely
        } else {
            KeepAlive::Until {
                time: self.args.keep_alive as u64,
                unit: TimeUnit::Seconds,
            }
        }
    }

    fn get_system_prompt(&self) -> String {
        let mut prompt = r#"# Role: Moral Advocate for Lact-O-Sensus
You are a pedantic, health-obsessed system agent. Evaluate mutations for semantic and physical integrity.

## Protocol Invariants (ADR 008)
1. **Taxonomy**: Assign exactly one: Primary Flora, Animal Secretions, Flesh And Marrow, Shelf Stable Carbohydrates, Cultured Doughs, Liquefied Hydration, Condiments And Catalysts, Nutrient Sparse Commodities, Ethanol Solutions, Biomedical Maintenance, Sanitization And Utility, Anomalous Inputs.
2. **Category Hints**: The user provides a category hint. If the hint is absurdly incorrect (e.g., 'oat milk' labeled as 'Flesh And Marrow'), you MUST assign the correct category (e.g., 'Primary Flora') in your JSON response and APPROVE the mutation if it is healthy. Do NOT veto solely for a mismatched category hint.
3. **Authorized SI Symbols**: g, kg, lb, oz, ml, l, gal, fl_oz, units, dozen, pack, misc.
4. **Identity Split**: If a unit is NOT in the authorized list (e.g., 'carton', 'handful'), you MUST:
   - Use `resolved_unit: "misc"`
   - Append the unit slug to the `resolved_item_key` (e.g., 'oat_milk_carton').
5. **Multiplier Duty**: For contextual units like `pack`, `misc`, or `handful`, you MUST provide a decimal `custom_multiplier` representing the total quantity of the base unit in that container.
   - *Example*: "6-pack of 500ml water" -> `resolved_unit: "ml"`, `custom_multiplier: "3000"`.
   - *Example*: "12 pack of eggs" -> `resolved_unit: "units"`, `custom_multiplier: "12"`.
6. **Morality**: Reject re-stocking excessive junk food, cigarettes, or alcohol.
7. **Brevity**: Limit `moral_justification` to exactly 2 clinical sentences or under 200 characters.

## Output Format
Output ONLY raw JSON matching this schema:
{
  "is_approved": bool,
  "category_assignment": "Category",
  "moral_justification": "Rationale",
  "resolved_item_key": "snake_case_key",
  "suggested_display_name": "Name",
  "resolved_unit": "symbol",
  "custom_multiplier": "string_decimal"
}
Constraint: Do NOT invent units or categories. Use exactly the strings provided above."#.to_string();

        if !self.args.think {
            prompt.push_str("\n\n[FLASH MODE: SKIP ALL THINKING. RESPOND IMMEDIATELY WITH JSON]");
        }

        prompt
    }

    /// Validates operation and serializes a dense, token-efficient view of the
    /// inventory.
    fn build_user_prompt(&self, req: &EvaluateProposalRequest) -> Result<String, Status> {
        let intent = req.intent.as_ref().ok_or_else(|| {
            Status::invalid_argument("Evaluation request is missing mutation intent")
        })?;

        // Early Return: Validate Operation Type
        let op = OperationType::try_from(intent.operation).map_err(|_| {
            Status::invalid_argument(format!("Unknown operation code: {}", intent.operation))
        })?;

        if op == OperationType::Unspecified {
            return Err(Status::invalid_argument(
                "Operation type must be explicitly specified (Add, Sub, Set, or Delete)",
            ));
        }

        let mutation_desc = format!(
            "OP: {:?}, Item: {}, Qty: {}, Unit: {}, Cat: {}",
            op,
            intent.item_key,
            intent.quantity.as_deref().unwrap_or("N/A"),
            intent.unit.as_deref().unwrap_or("N/A"),
            intent.category.as_deref().unwrap_or("N/A")
        );

        // Dense Serialization: Only include fields necessary for moral/relational
        // judgement
        let inventory_summary = req
            .current_inventory
            .iter()
            .map(|i| format!("{}: {} {} ({})", i.item_key, i.quantity, i.unit, i.category))
            .collect::<Vec<_>>()
            .join(", ");

        Ok(format!(
            "### EVALUATION REQUEST\nMutation: {}\nInventory Context: [{}]",
            mutation_desc, inventory_summary
        ))
    }

    /// Performs a No-Op request to pin the model in VRAM and ensure Ollama is
    /// reachable.
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        info!("Performing VRAM warm-up for model '{}'...", self.args.model);
        let messages = vec![ChatMessage::new(MessageRole::User, "ping".to_string())];
        let req =
            ChatMessageRequest::new(self.args.model.clone(), messages).think(ThinkType::False);

        self.ollama.send_chat_messages(req).await?;
        info!("Warm-up complete. Model is pinned in VRAM.");
        Ok(())
    }

    /// Validates AI metadata against system registries.
    /// Returns Status::internal if a hallucination is detected to trigger
    /// Gateway retry.
    fn validate_semantic_integrity(&self, res: &LlmResponse) -> Result<(), Status> {
        // 1. Validate Category
        if GroceryCategory::from_str(&res.category_assignment).is_err() {
            warn!("AI Hallucinated category: '{}'", res.category_assignment);
            return Err(Status::internal("AI Hallucination: Invalid Category"));
        }

        // 2. Validate Unit Symbol
        if UnitRegistry::resolve_symbol(&res.resolved_unit).is_err() {
            warn!("AI Hallucinated unit: '{}'", res.resolved_unit);
            return Err(Status::internal("AI Hallucination: Invalid Unit"));
        }

        // 3. Prevent empty keys
        if res.resolved_item_key.trim().is_empty() {
            warn!("AI returned empty item key.");
            return Err(Status::internal("AI Hallucination: Empty Item Key"));
        }

        Ok(())
    }
}

#[tonic::async_trait]
impl PolicyService for RealPolicyService {
    async fn evaluate_proposal(
        &self,
        request: Request<EvaluateProposalRequest>,
    ) -> Result<Response<EvaluateProposalResponse>, Status> {
        let req = request.into_inner();
        let chat_req = self.build_chat_request(&req)?;

        info!(
            "Evaluating proposal from client={} for item_key='{}'",
            req.client_id,
            req.intent
                .as_ref()
                .map(|i| i.item_key.as_str())
                .unwrap_or("none")
        );

        let start = std::time::Instant::now();
        let res = self
            .ollama
            .send_chat_messages(chat_req)
            .await
            .map_err(|e| {
                error!("Ollama chat failed after {:?}: {}", start.elapsed(), e);
                Status::internal(format!("LLM Error: {}", e))
            })?;

        let duration = start.elapsed();
        info!("LLM responded in {:?}", duration);

        let message_content = res.message.content.trim();

        // Scrub markdown backticks if present
        let clean_json = if message_content.starts_with("```") {
            message_content
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim()
        } else {
            message_content
        };

        if clean_json.is_empty() {
            error!("AI returned empty response content.");
            return Err(Status::internal("AI Hallucination: Empty response"));
        }

        let llm_res: LlmResponse = serde_json::from_str(clean_json).map_err(|e| {
            error!(
                "Failed to parse LLM JSON: {}. Raw: {}",
                e, res.message.content
            );
            Status::internal("AI Hallucination: Malformed response format")
        })?;

        // Semantic Integrity Guard
        self.validate_semantic_integrity(&llm_res)?;

        info!(
            "Evaluation complete. Approved: {}, Resolved Key: '{}', Category: '{}'",
            llm_res.is_approved, llm_res.resolved_item_key, llm_res.category_assignment
        );

        // Resolve stabilization multiplier (ADR 008)
        let multiplier = if let Some(ref custom) = llm_res.custom_multiplier {
            custom.clone()
        } else {
            UnitRegistry::resolve_symbol(&llm_res.resolved_unit)
                .map(|e| e.multiplier.to_string())
                .unwrap_or_else(|_| "1.0".to_string())
        };

        Ok(Response::new(EvaluateProposalResponse {
            is_approved: llm_res.is_approved,
            category_assignment: llm_res.category_assignment,
            moral_justification: llm_res.moral_justification,
            resolved_item_key: llm_res.resolved_item_key,
            suggested_display_name: llm_res.suggested_display_name,
            resolved_unit: llm_res.resolved_unit,
            conversion_multiplier_to_base: multiplier,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_args(think: bool) -> Args {
        Args {
            port: 50060,
            model: "test-model".to_string(),
            ollama_host: "http://localhost".to_string(),
            ollama_port: 11434,
            think,
            keep_alive: -1,
            max_tokens: 256,
        }
    }

    mod build_chat_request {
        use common::proto::v1::app::EvaluateProposalRequest;

        use super::*;

        #[test]
        fn specifies_reasoning_state_based_on_think_flag() {
            let service_no_think = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(common::proto::v1::app::MutationIntent {
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };
            match service_no_think.build_chat_request(&req).unwrap().think {
                Some(ThinkType::False) => (),
                _ => panic!("Expected Some(ThinkType::False)"),
            }

            let service_think = RealPolicyService::new(test_args(true));
            match service_think.build_chat_request(&req).unwrap().think {
                Some(ThinkType::True) => (),
                _ => panic!("Expected Some(ThinkType::True)"),
            }
        }

        #[test]
        fn enforces_strict_isolation_by_sending_exactly_two_messages() {
            let service = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(common::proto::v1::app::MutationIntent {
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let chat_req = service.build_chat_request(&req).unwrap();

            assert_eq!(chat_req.messages.len(), 2);
            assert_eq!(chat_req.messages[0].role, MessageRole::System);
            assert_eq!(chat_req.messages[1].role, MessageRole::User);
        }

        #[test]
        fn enforces_json_mode_for_caching_and_formatting() {
            let service = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(common::proto::v1::app::MutationIntent {
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let chat_req = service.build_chat_request(&req).unwrap();
            assert!(matches!(chat_req.format, Some(FormatType::Json)));
        }

        #[test]
        fn mandates_clinical_brevity_in_system_prompt() {
            let service = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(common::proto::v1::app::MutationIntent {
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let chat_req = service.build_chat_request(&req).unwrap();
            assert!(chat_req.messages[0].content.contains("Brevity"));
            assert!(chat_req.messages[0].content.contains("200 characters"));
        }
    }

    mod build_user_prompt {
        use common::proto::v1::app::EvaluateProposalRequest;
        use common::proto::v1::app::GroceryItem;
        use common::proto::v1::app::MutationIntent;

        use super::*;

        #[test]
        fn serializes_inventory_densely_by_excluding_unnecessary_metadata() {
            let service = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(MutationIntent {
                    item_key: "oat_milk".to_string(),
                    operation: OperationType::Add as i32,
                    ..Default::default()
                }),
                current_inventory: vec![GroceryItem {
                    item_key: "apple".to_string(),
                    quantity: "5".to_string(),
                    unit: "units".to_string(),
                    category: "Primary Flora".to_string(),
                    last_modifier_id: "client-1".to_string(), // Should be excluded
                    ..Default::default()
                }],
                ..Default::default()
            };

            let prompt = service.build_user_prompt(&req).unwrap();
            assert!(prompt.contains("apple: 5 units (Primary Flora)"));
            assert!(!prompt.contains("client-1")); // Metadata must be pruned
        }

        #[test]
        fn rejects_unspecified_operation_early() {
            let service = RealPolicyService::new(test_args(false));
            let req = EvaluateProposalRequest {
                intent: Some(MutationIntent {
                    operation: OperationType::Unspecified as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };

            let result = service.build_user_prompt(&req);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    mod validate_semantic_integrity {
        use super::*;

        #[test]
        fn accepts_valid_registry_metadata() {
            let service = RealPolicyService::new(test_args(false));
            let res = LlmResponse {
                is_approved: true,
                category_assignment: "Liquefied Hydration".to_string(),
                moral_justification: "OK".to_string(),
                resolved_item_key: "oat_milk".to_string(),
                suggested_display_name: "Oat Milk".to_string(),
                resolved_unit: "ml".to_string(),
                custom_multiplier: None,
            };
            assert!(service.validate_semantic_integrity(&res).is_ok());
        }

        #[test]
        fn rejects_hallucinated_categories_with_internal_error() {
            let service = RealPolicyService::new(test_args(false));
            let res = LlmResponse {
                is_approved: true,
                category_assignment: "Bakery".to_string(), // Not in enum
                moral_justification: "OK".to_string(),
                resolved_item_key: "oat_milk".to_string(),
                suggested_display_name: "Oat Milk".to_string(),
                resolved_unit: "ml".to_string(),
                custom_multiplier: None,
            };
            let status = service.validate_semantic_integrity(&res).unwrap_err();
            assert_eq!(status.code(), tonic::Code::Internal);
            assert!(status.message().contains("Invalid Category"));
        }

        #[test]
        fn rejects_hallucinated_units_with_internal_error() {
            let service = RealPolicyService::new(test_args(false));
            let res = LlmResponse {
                is_approved: true,
                category_assignment: "Liquefied Hydration".to_string(),
                moral_justification: "OK".to_string(),
                resolved_item_key: "oat_milk".to_string(),
                suggested_display_name: "Oat Milk".to_string(),
                resolved_unit: "cartons".to_string(), // Not in registry
                custom_multiplier: None,
            };
            let status = service.validate_semantic_integrity(&res).unwrap_err();
            assert_eq!(status.code(), tonic::Code::Internal);
            assert!(status.message().contains("Invalid Unit"));
        }
    }
}
