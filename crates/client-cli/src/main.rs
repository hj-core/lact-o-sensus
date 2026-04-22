use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use client_cli::client::LactoClient;
use client_cli::repl;
use client_cli::state::ClientState;
use common::types::ClusterId;
use tokio::io::BufReader;
use tokio::io::stdin;
use tokio::io::stdout;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;

/// Lact-O-Sensus: A Distributed, AI-Vetoed Grocery State Machine
///
/// The Smart Client provides linearizable reads, Exactly-Once Semantics,
/// and automatic leader redirection.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The unique identifier of the Lact-O-Sensus cluster to join.
    #[arg(short, long, env = "LACTO_CLUSTER_ID")]
    cluster_id: String,

    /// Initial seed nodes to bootstrap discovery (repeatable).
    #[arg(short, long, env = "LACTO_SEEDS")]
    seed: Vec<String>,

    /// Path to the local persistent session state file.
    #[arg(short = 'p', long, default_value = ".client_state.json")]
    state_path: PathBuf,

    /// Path to the Write-Ahead Log directory.
    #[arg(short = 'w', long, default_value = ".client_wal")]
    wal_path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize diagnostics
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_thread_ids(false)
        .init();

    let args = Cli::parse();
    let cluster_id = ClusterId::try_new(&args.cluster_id)?;

    info!(
        "Bootstrapping Lact-O-Sensus client for cluster '{}'...",
        cluster_id
    );

    // 2. Initialize or load persistent session state (EOS Mandate)
    let state = ClientState::load_or_init(args.state_path, cluster_id, args.seed)?;

    // 3. Initialize resilient gRPC client with durable WAL
    let client = LactoClient::new(state, args.wal_path)?;

    // 4. Perform Startup Recovery (ADR 001)
    // Synchronously reconcile any pending intents before accepting new commands.
    recover_pending_intents(&client).await?;

    // 5. Start interactive REPL loop
    println!("Welcome to Lact-O-Sensus. Type 'exit' to quit.");
    repl::run_repl(&client, BufReader::new(stdin()), &mut stdout()).await?;

    info!("Session terminated gracefully.");
    Ok(())
}

/// Recovers and re-proposes all mutation intents stored in the WAL.
///
/// This ensures that intents interrupted by a client crash are eventually
/// processed by the cluster, fulfilling the Exactly-Once Semantics guarantee.
async fn recover_pending_intents(client: &LactoClient) -> Result<()> {
    let pending = client.wal().recover()?;
    if pending.is_empty() {
        return Ok(());
    }

    warn!(
        "Found {} pending intents in WAL. Starting recovery reconciliation...",
        pending.len()
    );

    for (seq, req) in pending {
        info!("Recovering intent sequence {}...", seq);
        match client.repropose_mutation(seq, req).await {
            Ok(res) => {
                info!(
                    "Successfully recovered intent {}. Status: {:?}",
                    seq, res.status
                );
            }
            Err(e) => {
                error!(
                    "Failed to recover intent {}: {:#}. Manual intervention may be required if \
                     the cluster is unreachable.",
                    seq, e
                );
                // We halt recovery if we can't even talk to the cluster,
                // as ordering must be preserved.
                return Err(e);
            }
        }
    }

    info!("Recovery reconciliation complete.");
    Ok(())
}
