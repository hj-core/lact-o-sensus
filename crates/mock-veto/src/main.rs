use std::net::SocketAddr;

use clap::Parser;
use common::proto::v1::app::EvaluateProposalRequest;
use common::proto::v1::app::EvaluateProposalResponse;
use common::proto::v1::app::policy_service_server::PolicyService;
use common::proto::v1::app::policy_service_server::PolicyServiceServer;
use common::types::trace::ClinicalTarget;
use common_rpc::TraceInterceptor;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::transport::Server;
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value_t = 50060)]
    pub port: u16,
}

#[derive(Default)]
pub struct MockPolicyService {}

#[tonic::async_trait]
impl PolicyService for MockPolicyService {
    async fn evaluate_proposal(
        &self,
        request: Request<EvaluateProposalRequest>,
    ) -> Result<Response<EvaluateProposalResponse>, Status> {
        let trace_id = TraceInterceptor::require_trace_id(&request).or_else(|_| {
            // Manual extraction for Mock (where we don't have a server-side interceptor
            // layer)
            request
                .metadata()
                .get("x-trace-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Status::failed_precondition("Missing TraceId in mock metadata"))
        })?;
        let req = request.into_inner();

        let intent = req.intent.ok_or_else(|| {
            Status::invalid_argument("EvaluateProposalRequest is missing 'intent' field")
        })?;

        let mut response = Response::new(EvaluateProposalResponse::new(
            true, // is_approved
            intent.category.clone().unwrap_or_default(),
            "Mocked Approval".to_string(),
            intent.item_key.clone(),
            intent.item_key.clone(),
            intent.unit.clone().unwrap_or_default(),
            "1.0".to_string(), // multiplier
        ));
        TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)?;
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));

    info!(
        target: ClinicalTarget::ClinicalOracle.as_str(),
        port = args.port,
        "Starting Mock Veto Node (Insta-Approve)..."
    );

    let service = PolicyServiceServer::new(MockPolicyService::default());

    Server::builder().add_service(service).serve(addr).await?;

    Ok(())
}
