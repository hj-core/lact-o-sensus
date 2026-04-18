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
use tracing::info;
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

    // 3. Initialize resilient gRPC client
    let client = LactoClient::new(state);

    // 4. Start interactive REPL loop
    println!("Welcome to Lact-O-Sensus. Type 'exit' to quit.");
    repl::run_repl(&client, BufReader::new(stdin()), &mut stdout()).await?;

    info!("Session terminated gracefully.");
    Ok(())
}
