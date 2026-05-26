//! Composition Root for the AI Moral Advocate.
//!
//! This binary initializes the clinical oracle environment, wires the
//! `RealPolicyService` with gRPC interceptors, and performs proactive VRAM
//! warm-up to ensure low-latency semantic evaluation during the mutation
//! lifecycle.

pub mod args;
pub mod model;
pub mod service;

use std::net::SocketAddr;

use clap::Parser;
use common::proto::v1::app::policy_service_server::PolicyServiceServer;
use common::rpc::TraceInterceptor;
use common::types::trace::ClinicalTarget;
use tonic::transport::Server;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_subscriber::EnvFilter;

use crate::args::Args;
use crate::service::RealPolicyService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));

    let root_span = info_span!(
        target: ClinicalTarget::ClinicalFoundation.as_str(),
        "ai_veto",
        port = args.port,
        model = %args.model
    );

    async move {
        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "AI Moral Advocate (ADR 008 Compliant) starting on {}", addr
        );

        let shutdown = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    "Failed to install CTRL+C handler: {}", e
                );
            } else {
                info!(
                    target: ClinicalTarget::ClinicalFoundation.as_str(),
                    "Shutdown signal received. Commencing graceful exit..."
                );
            }
        };

        let service = RealPolicyService::new(args);

        // Proactive VRAM Warm-up (Task 2)
        if let Err(e) = service.warm_up().await {
            error!(
                target: ClinicalTarget::ClinicalFoundation.as_str(),
                "Fatal Error during model warm-up: {}. Verify Ollama is running and model is \
                 pulled.",
                e
            );
            return Err(anyhow::anyhow!("Warm-up failed"));
        }

        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "Service initialized, starting gRPC server..."
        );

        let service =
            PolicyServiceServer::with_interceptor(service, TraceInterceptor::propagative());

        Server::builder()
            .add_service(service)
            .serve_with_shutdown(addr, shutdown)
            .await?;

        info!(
            target: ClinicalTarget::ClinicalFoundation.as_str(),
            "AI Moral Advocate lifecycle finished. Goodbye."
        );
        Ok(())
    }
    .instrument(root_span)
    .await
}
