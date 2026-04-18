use std::fmt;

use anyhow::Result;
use common::proto::v1::MutationIntent;
use common::proto::v1::MutationStatus;
use common::proto::v1::OperationType;
use common::proto::v1::QueryStatus;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::client::LactoClient;

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
    fn parse(args: &[String], cmd_name: &str) -> Result<Self> {
        if args.len() < 2 {
            anyhow::bail!(
                "Usage: {} <item_key> <quantity> [unit] [category]",
                cmd_name
            );
        }
        if args.len() > 4 {
            anyhow::bail!(
                "Too many arguments for '{}'. Expected at most 4, found {}.",
                cmd_name,
                args.len()
            );
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
        MutationIntent {
            item_key: self.item_key,
            quantity: self.quantity,
            unit: self.unit,
            category: self.category,
            operation: operation as i32,
        }
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
    Query { filter: Option<String> },
    /// Graceful Termination: Exits the REPL loop.
    Exit,
}

impl Command {
    /// Parses a raw user string into a structured `Command`.
    pub fn parse(input: &str) -> Result<Self> {
        let tokens = shlex::split(input)
            .ok_or_else(|| anyhow::anyhow!("Incomplete or malformed quoting in command"))?;

        if tokens.is_empty() {
            anyhow::bail!("Empty command");
        }

        let cmd_name = tokens[0].to_lowercase();
        let args = &tokens[1..];

        match cmd_name.as_str() {
            "add" => Ok(Self::Add(MutationArgs::parse(args, "add")?)),
            "subtract" | "sub" => Ok(Self::Subtract(MutationArgs::parse(args, "subtract")?)),
            "set" => Ok(Self::Set(MutationArgs::parse(args, "set")?)),
            "delete" | "del" | "rm" => {
                if args.is_empty() {
                    anyhow::bail!("Usage: delete <item_key>");
                }
                if args.len() > 1 {
                    anyhow::bail!(
                        "Too many arguments for 'delete'. Expected 1, found {}.",
                        args.len()
                    );
                }
                Ok(Self::Delete {
                    item_key: args[0].clone(),
                })
            }
            "query" | "ls" => {
                if args.len() > 1 {
                    anyhow::bail!(
                        "Too many arguments for 'query'. Expected at most 1, found {}.",
                        args.len()
                    );
                }
                Ok(Self::Query {
                    filter: args.first().cloned(),
                })
            }
            "exit" | "quit" | "q" => {
                if !args.is_empty() {
                    anyhow::bail!("'exit' does not take arguments.");
                }
                Ok(Self::Exit)
            }
            _ => anyhow::bail!("Unknown command: '{}'. Type 'exit' to quit.", cmd_name),
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
            Command::Query { filter } => {
                write!(f, "QUERY")?;
                if let Some(filt) = filter {
                    write!(f, " (filter: '{}')", filt)?;
                }
                Ok(())
            }
            Command::Exit => write!(f, "EXIT"),
        }
    }
}

/// The main interactive loop for the Lact-O-Sensus client.
pub async fn run_repl<R, W>(client: &LactoClient, mut reader: R, writer: &mut W) -> Result<()>
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

async fn execute_command(client: &LactoClient, cmd: Command) -> Result<String> {
    match cmd {
        Command::Add(args) => {
            let res = client
                .propose_mutation(args.into_intent(OperationType::Add))
                .await?;
            format_mutation_response(res)
        }
        Command::Subtract(args) => {
            let res = client
                .propose_mutation(args.into_intent(OperationType::Subtract))
                .await?;
            format_mutation_response(res)
        }
        Command::Set(args) => {
            let res = client
                .propose_mutation(args.into_intent(OperationType::Set))
                .await?;
            format_mutation_response(res)
        }
        Command::Delete { item_key } => {
            let intent = MutationIntent {
                item_key,
                quantity: "0".to_string(),
                unit: None,
                category: None,
                operation: OperationType::Delete as i32,
            };
            let res = client.propose_mutation(intent).await?;
            format_mutation_response(res)
        }
        Command::Query { filter } => {
            let res = client.query_state(filter, None).await?;
            format_query_response(res)
        }
        Command::Exit => unreachable!(),
    }
}

fn format_mutation_response(res: common::proto::v1::ProposeMutationResponse) -> Result<String> {
    match MutationStatus::try_from(res.status) {
        Ok(MutationStatus::Committed) => Ok(format!(
            "SUCCESS: Committed at version {}",
            res.state_version
        )),
        Ok(MutationStatus::Vetoed) => Ok(format!("VETOED: {}", res.error_message)),
        Ok(MutationStatus::Rejected) => Ok(format!("REJECTED: {}", res.error_message)),
        _ => Ok(format!("UNKNOWN STATUS: {}", res.status)),
    }
}

fn format_query_response(res: common::proto::v1::QueryStateResponse) -> Result<String> {
    match QueryStatus::try_from(res.status) {
        Ok(QueryStatus::Success) => {
            if res.items.is_empty() {
                return Ok("Inventory is empty.".to_string());
            }
            let mut output = format!("Inventory (version: {}):\n", res.current_state_version);
            for item in res.items {
                output.push_str(&format!(
                    "  - {} ({} {})\n",
                    item.item_key, item.quantity, item.unit
                ));
            }
            Ok(output.trim_end().to_string())
        }
        Ok(QueryStatus::Rejected) => Ok(format!("REJECTED: {}", res.error_message)),
        _ => Ok(format!("ERROR: {}", res.error_message)),
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

        #[test]
        fn parses_basic_mutations() {
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
        fn handles_quoted_strings() {
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
        fn parses_optional_arguments() {
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
        fn parses_delete_and_query() {
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
                    filter: Some(".*dairy.*".to_string())
                }
            );
        }

        #[test]
        fn rejects_excessive_arguments() {
            let res = Command::parse("delete milk eggs");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("Too many"));
        }

        #[test]
        fn handles_malformed_input() {
            let res = Command::parse("add \"unclosed quote");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("quoting"));
        }
    }

    mod run_repl {
        use super::*;

        #[tokio::test]
        async fn handles_exit_command() -> Result<()> {
            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                ClusterId::try_new("test")?,
                vec!["127.0.0.1:1".to_string()],
            )?;
            let client = LactoClient::new(state);

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
