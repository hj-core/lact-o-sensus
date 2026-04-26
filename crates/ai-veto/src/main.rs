use std::net::SocketAddr;

use clap::Parser;
use common::proto::v1::app::EvaluateProposalRequest;
use common::proto::v1::app::EvaluateProposalResponse;
use common::proto::v1::app::policy_service_server::PolicyService;
use common::proto::v1::app::policy_service_server::PolicyServiceServer;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::transport::Server;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_subscriber::EnvFilter;

/// The AI Veto Node (Mock Mode)
///
/// This node provides automated evaluation of grocery mutations.
/// In Phase 4, it operates in mock mode, approving all requests.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 50060)]
    port: u16,
}

#[derive(Debug, Default)]
pub struct MockPolicyService;

#[tonic::async_trait]
impl PolicyService for MockPolicyService {
    async fn evaluate_proposal(
        &self,
        request: Request<EvaluateProposalRequest>,
    ) -> Result<Response<EvaluateProposalResponse>, Status> {
        let req = request.into_inner();
        let intent = req.intent.clone().unwrap_or_default();
        info!(
            "Evaluating proposal from client={} for item='{}'",
            req.client_id, intent.item_key
        );

        // Deterministic Mock Approval
        Ok(Response::new(EvaluateProposalResponse {
            is_approved: true,
            category_assignment: "Anomalous Inputs".to_string(), // Default mock category
            moral_justification: "Mock approval for Phase 5 semantic resolution verification."
                .to_string(),
            resolved_item_key: intent.item_key,
            suggested_display_name: "Mock Item".to_string(),
            resolved_unit: "g".to_string(),
            conversion_multiplier_to_base: "1.0".to_string(),
        }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize logging with Aligned Rigor
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));

    // 2. Create the Root Span
    let root_span = info_span!("ai_veto", port = args.port);

    async move {
        info!("AI Veto Node starting on {}", addr);

        // Define the graceful shutdown signal
        let shutdown = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Failed to install CTRL+C handler: {}", e);
            } else {
                info!("Shutdown signal received. Commencing graceful exit...");
            }
        };

        // 3. Start gRPC Server
        let service = MockPolicyService::default();

        Server::builder()
            .add_service(PolicyServiceServer::new(service))
            .serve_with_shutdown(addr, shutdown)
            .await?;

        info!("AI Veto Node lifecycle finished. Goodbye.");
        Ok(())
    }
    .instrument(root_span)
    .await
}
