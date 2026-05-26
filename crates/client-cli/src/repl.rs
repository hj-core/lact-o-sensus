//! Interactive user interface (REPL) for the Lact-O-Sensus client.
//!
//! This module implements the command-line interface, providing an interactive
//! shell for users to issue inventory mutations and queries. It handles raw
//! string parsing, quoting, and routes syntactically valid commands to the
//! underlying `LactoClient` orchestrator.

use std::fmt;

use common::proto::v1::MutationIntent;
use common::proto::v1::MutationStatus;
use common::proto::v1::OperationType;
use common::proto::v1::QueryStatus;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateResponse;
use common::types::LogIndex;
use common::types::trace::TraceId;
use thiserror::Error;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::client::ClientError;
use crate::client::LactoClient;

/// Errors associated with REPL operations and command parsing.
#[derive(Debug, Error)]
pub enum ReplError {
    #[error("Syntax Error: {0}")]
    Syntax(String),

    #[error("Usage: {0}")]
    Usage(String),

    #[error("Network or Protocol Error: {0}")]
    Client(#[from] ClientError),

    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),

    #[error("Execution Error: {0}")]
    Execution(String),
}

/// Shared arguments for mutations that modify item quantities.
#[derive(Debug, PartialEq, Clone)]
pub struct MutationArgs {
    pub item_key: String,
    pub quantity: String,
    pub unit: Option<String>,
    pub category: Option<String>,
}

impl MutationArgs {
    /// Parses mutation arguments from a token slice.
    fn parse(args: &[String], cmd_name: &str) -> Result<Self, ReplError> {
        if args.len() < 2 {
            return Err(ReplError::Usage(format!(
                "{} <item_key> <quantity> [unit] [category]",
                cmd_name
            )));
        }
        if args.len() > 4 {
            return Err(ReplError::Syntax(format!(
                "Too many arguments for '{}'. Expected at most 4, found {}.",
                cmd_name,
                args.len()
            )));
        }

        Ok(Self {
            item_key: args[0].clone(),
            quantity: args[1].clone(),
            unit: args.get(2).cloned(),
            category: args.get(3).cloned(),
        })
    }

    /// Converts the arguments into a protocol intent.
    fn into_intent(self, operation: OperationType) -> MutationIntent {
        MutationIntent::new(
            self.item_key,
            Some(self.quantity),
            self.unit,
            self.category,
            operation,
        )
    }
}

impl fmt::Display for MutationArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.item_key, self.quantity)?;
        if let Some(u) = &self.unit {
            write!(f, " {}", u)?;
        }
        if let Some(c) = &self.category {
            write!(f, " (category: {})", c)?;
        }
        Ok(())
    }
}

/// A structured representation of an interactive user command.
#[derive(Debug, PartialEq, Clone)]
pub enum Command {
    /// Incremental: Adds a quantity to an item.
    Add(MutationArgs),
    /// Decremental: Subtracts a quantity from an item.
    Subtract(MutationArgs),
    /// Absolute: Overwrites the current quantity of an item.
    Set(MutationArgs),
    /// Removal: Removes an item from the inventory.
    Delete { item_key: String },
    /// Linearizable Read: Queries the cluster state, optionally filtered.
    Query {
        filter: Option<String>,
        min_state_version: Option<u64>,
    },
    /// Graceful Termination: Exits the REPL loop.
    Exit,
}

impl Command {
    /// Parses a raw user string into a structured `Command`.
    pub fn parse(input: &str) -> Result<Self, ReplError> {
        let tokens = shlex::split(input).ok_or_else(|| {
            ReplError::Syntax("Incomplete or malformed quoting in command".into())
        })?;

        if tokens.is_empty() {
            return Err(ReplError::Syntax("Empty command".into()));
        }

        let cmd_name = tokens[0].to_lowercase();
        let args = &tokens[1..];

        match cmd_name.as_str() {
            "add" => Ok(Self::Add(MutationArgs::parse(args, "add")?)),
            "subtract" | "sub" => Ok(Self::Subtract(MutationArgs::parse(args, "subtract")?)),
            "set" => Ok(Self::Set(MutationArgs::parse(args, "set")?)),
            "delete" | "del" | "rm" => {
                if args.is_empty() {
                    return Err(ReplError::Usage("delete <item_key>".into()));
                }
                if args.len() > 1 {
                    return Err(ReplError::Syntax(format!(
                        "Too many arguments for 'delete'. Expected 1, found {}.",
                        args.len()
                    )));
                }
                Ok(Self::Delete {
                    item_key: args[0].clone(),
                })
            }
            "query" | "ls" => {
                if args.len() > 2 {
                    return Err(ReplError::Syntax(format!(
                        "Too many arguments for 'query'. Expected at most 2, found {}.",
                        args.len()
                    )));
                }
                let filter = args.first().cloned();
                let min_state_version = if let Some(v_str) = args.get(1) {
                    Some(v_str.parse::<u64>().map_err(|e| {
                        ReplError::Syntax(format!("Invalid min_state_version '{}': {}", v_str, e))
                    })?)
                } else {
                    None
                };
                Ok(Self::Query {
                    filter,
                    min_state_version,
                })
            }
            "exit" | "quit" | "q" => {
                if !args.is_empty() {
                    return Err(ReplError::Syntax("'exit' does not take arguments.".into()));
                }
                Ok(Self::Exit)
            }
            _ => Err(ReplError::Syntax(format!(
                "Unknown command: '{}'. Type 'exit' to quit.",
                cmd_name
            ))),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Add(args) => write!(f, "ADD {}", args),
            Command::Subtract(args) => write!(f, "SUBTRACT {}", args),
            Command::Set(args) => write!(f, "SET {}", args),
            Command::Delete { item_key } => write!(f, "DELETE {}", item_key),
            Command::Query {
                filter,
                min_state_version,
            } => {
                write!(f, "QUERY")?;
                if let Some(filt) = filter {
                    write!(f, " (filter: '{}')", filt)?;
                }
                if let Some(v) = min_state_version {
                    write!(f, " (min_version: {})", v)?;
                }
                Ok(())
            }
            Command::Exit => write!(f, "EXIT"),
        }
    }
}

/// The main interactive loop for the Lact-O-Sensus client.
pub async fn run_repl<R, W>(
    client: &LactoClient,
    mut reader: R,
    writer: &mut W,
) -> Result<(), ReplError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        writer.write_all(b"lacto> ").await?;
        writer.flush().await?;

        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        let cmd = match Command::parse(input) {
            Ok(Command::Exit) => break,
            Ok(c) => c,
            Err(e) => {
                writer
                    .write_all(format!("Error: {}\n", e).as_bytes())
                    .await?;
                continue;
            }
        };

        match execute_command(client, cmd).await {
            Ok(output) => {
                writer.write_all(format!("{}\n", output).as_bytes()).await?;
            }
            Err(e) => {
                writer
                    .write_all(format!("Network Error: {}\n", e).as_bytes())
                    .await?;
            }
        }
    }

    Ok(())
}

async fn execute_command(client: &LactoClient, cmd: Command) -> Result<String, ReplError> {
    match cmd {
        Command::Add(args) => {
            let (res, tid) = client
                .propose_mutation(args.into_intent(OperationType::Add))
                .await?;
            Ok(format_mutation_response(res, tid))
        }
        Command::Subtract(args) => {
            let (res, tid) = client
                .propose_mutation(args.into_intent(OperationType::Subtract))
                .await?;
            Ok(format_mutation_response(res, tid))
        }
        Command::Set(args) => {
            let (res, tid) = client
                .propose_mutation(args.into_intent(OperationType::Set))
                .await?;
            Ok(format_mutation_response(res, tid))
        }
        Command::Delete { item_key } => {
            let intent = MutationIntent::new(item_key, None, None, None, OperationType::Delete);
            let (res, tid) = client.propose_mutation(intent).await?;
            Ok(format_mutation_response(res, tid))
        }
        Command::Query {
            filter,
            min_state_version,
        } => {
            let (res, tid) = client
                .query_state(filter, min_state_version.map(LogIndex::new))
                .await?;
            Ok(format_query_response(res, tid))
        }
        Command::Exit => unreachable!(),
    }
}

fn format_mutation_response(res: ProposeMutationResponse, tid: Option<TraceId>) -> String {
    let trace_suffix = tid.map(|t| format!(" [Trace: {}]", t)).unwrap_or_default();

    match MutationStatus::try_from(res.status) {
        Ok(MutationStatus::Committed) => format!(
            "SUCCESS: Committed at version {}{}",
            res.state_version, trace_suffix
        ),
        Ok(MutationStatus::Vetoed) => format!("VETOED: {}{}", res.error_message, trace_suffix),
        Ok(MutationStatus::Rejected) => format!("REJECTED: {}{}", res.error_message, trace_suffix),
        _ => format!("UNKNOWN STATUS: {}{}", res.status, trace_suffix),
    }
}

fn format_query_response(res: QueryStateResponse, tid: Option<TraceId>) -> String {
    let trace_suffix = tid.map(|t| format!(" [Trace: {}]", t)).unwrap_or_default();

    match QueryStatus::try_from(res.status) {
        Ok(QueryStatus::Success) => {
            if res.items.is_empty() {
                return format!("Inventory is empty.{}", trace_suffix);
            }
            let mut output = format!("Inventory (version: {}):\n", res.current_state_version);
            for item in res.items {
                output.push_str(&format!(
                    "  - {} ({} {})\n",
                    item.item_key, item.quantity, item.unit
                ));
            }
            output.push_str(&trace_suffix);
            output.trim_end().to_string()
        }
        Ok(QueryStatus::Rejected) => format!("REJECTED: {}{}", res.error_message, trace_suffix),
        _ => format!("ERROR: {}{}", res.error_message, trace_suffix),
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;
    use tempfile::tempdir;
    use tokio::io::duplex;

    use super::*;
    use crate::state::ClientState;

    mod command_parse {
        use super::*;

        mod input_tokens {
            use super::*;

            #[test]
            fn parses_basic_mutations_when_tokens_are_valid() {
                let cmd = Command::parse("add milk 2").unwrap();
                assert_eq!(
                    cmd,
                    Command::Add(MutationArgs {
                        item_key: "milk".to_string(),
                        quantity: "2".to_string(),
                        unit: None,
                        category: None
                    })
                );
            }

            #[test]
            fn handles_quoted_strings_when_using_shell_splitting() {
                let cmd = Command::parse("add \"organic milk\" 2").unwrap();
                assert_eq!(
                    cmd,
                    Command::Add(MutationArgs {
                        item_key: "organic milk".to_string(),
                        quantity: "2".to_string(),
                        unit: None,
                        category: None
                    })
                );
            }

            #[test]
            fn parses_optional_arguments_when_provided_fully() {
                let cmd = Command::parse("set bread 2 loaves bakery").unwrap();
                assert_eq!(
                    cmd,
                    Command::Set(MutationArgs {
                        item_key: "bread".to_string(),
                        quantity: "2".to_string(),
                        unit: Some("loaves".to_string()),
                        category: Some("bakery".to_string())
                    })
                );
            }

            #[test]
            fn parses_delete_and_query_when_invoked_with_standard_aliases() {
                let cmd = Command::parse("delete milk").unwrap();
                assert_eq!(
                    cmd,
                    Command::Delete {
                        item_key: "milk".to_string()
                    }
                );

                let cmd = Command::parse("query \".*dairy.*\"").unwrap();
                assert_eq!(
                    cmd,
                    Command::Query {
                        filter: Some(".*dairy.*".to_string()),
                        min_state_version: None
                    }
                );

                let cmd = Command::parse("query milk 10").unwrap();
                assert_eq!(
                    cmd,
                    Command::Query {
                        filter: Some("milk".to_string()),
                        min_state_version: Some(10)
                    }
                );
            }
        }

        mod syntax_validation {
            use super::*;

            #[test]
            fn rejects_invalid_min_state_version_when_non_numeric() {
                let res = Command::parse("query milk abc");
                assert!(res.is_err());
                assert!(res.unwrap_err().to_string().contains("min_state_version"));
            }

            #[test]
            fn rejects_excessive_arguments_when_count_is_exceeded() {
                let res = Command::parse("delete milk eggs");
                assert!(res.is_err());
                assert!(res.unwrap_err().to_string().contains("Too many"));
            }

            #[test]
            fn handles_malformed_input_when_quotes_are_unclosed() {
                let res = Command::parse("add \"unclosed quote");
                assert!(res.is_err());
                assert!(res.unwrap_err().to_string().contains("quoting"));
            }
        }
    }

    mod intent_transformation {
        use super::*;

        mod argument_lifting {
            use super::*;

            #[test]
            fn wraps_quantity_in_some_when_converting_mutations() {
                let args = MutationArgs {
                    item_key: "milk".to_string(),
                    quantity: "1.5".to_string(),
                    unit: None,
                    category: None,
                };
                let intent = args.into_intent(OperationType::Add);
                assert_eq!(intent.quantity, Some("1.5".to_string()));
            }

            #[test]
            fn uses_none_quantity_when_constructing_delete_intent() {
                // Setup: Construct a Delete command
                let cmd = Command::parse("delete milk").unwrap();

                // Logic: The REPL handler constructs the intent for Delete
                if let Command::Delete { item_key } = cmd {
                    let intent =
                        MutationIntent::new(item_key, None, None, None, OperationType::Delete);
                    assert_eq!(intent.quantity, None);
                } else {
                    panic!("Expected Delete command");
                }
            }
        }
    }

    mod run_repl {
        use super::*;

        mod lifecycle_management {
            use super::*;

            #[tokio::test]
            async fn handles_exit_command_when_received_via_input()
            -> Result<(), Box<dyn std::error::Error>> {
                let dir = tempdir()?;
                let state = ClientState::load_or_init(
                    dir.path().join("state.json"),
                    ClusterId::try_new("test")?,
                    vec!["127.0.0.1:1".to_string()],
                )?;
                let client = LactoClient::new(state, dir.path().join("wal"))?;

                let (client_side, mut server_side) = duplex(64);
                server_side.write_all(b"exit\n").await?;

                let repl_task = tokio::spawn(async move {
                    let mut out = Vec::new();
                    run_repl(&client, tokio::io::BufReader::new(client_side), &mut out)
                        .await
                        .unwrap();
                    out
                });

                let output = repl_task.await?;
                assert!(String::from_utf8(output)?.contains("lacto> "));
                Ok(())
            }
        }
    }
}
