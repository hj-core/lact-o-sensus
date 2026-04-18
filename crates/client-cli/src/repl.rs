use std::fmt;

/// Shared arguments for mutations that modify item quantities.
#[derive(Debug, PartialEq, Clone)]
pub struct MutationArgs {
    pub item_key: String,
    pub quantity: String,
    pub unit: Option<String>,
    pub category: Option<String>,
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
///
/// This enum maps human-readable REPL input to the underlying protocol
/// operations defined in ADR 005.
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

/// Parses a raw user string into a structured `Command`.
///
/// Utilizes `shlex` to handle quoted strings (e.g., `add "organic milk" 2`)
/// and maps commands case-insensitively.
pub fn parse_command(input: &str) -> anyhow::Result<Command> {
    let tokens = shlex::split(input)
        .ok_or_else(|| anyhow::anyhow!("Incomplete or malformed quoting in command"))?;

    if tokens.is_empty() {
        anyhow::bail!("Empty command");
    }

    let cmd_name = tokens[0].to_lowercase();
    let args = &tokens[1..];

    match cmd_name.as_str() {
        "add" => {
            let mutation_args = parse_mutation_args(args, "add")?;
            Ok(Command::Add(mutation_args))
        }
        "subtract" | "sub" => {
            let mutation_args = parse_mutation_args(args, "subtract")?;
            Ok(Command::Subtract(mutation_args))
        }
        "set" => {
            let mutation_args = parse_mutation_args(args, "set")?;
            Ok(Command::Set(mutation_args))
        }
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
            Ok(Command::Delete {
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
            let filter = args.first().cloned();
            Ok(Command::Query { filter })
        }
        "exit" | "quit" | "q" => {
            if !args.is_empty() {
                anyhow::bail!("'exit' does not take arguments.");
            }
            Ok(Command::Exit)
        }
        _ => anyhow::bail!("Unknown command: '{}'. Type 'exit' to quit.", cmd_name),
    }
}

fn parse_mutation_args(args: &[String], cmd_name: &str) -> anyhow::Result<MutationArgs> {
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

    Ok(MutationArgs {
        item_key: args[0].clone(),
        quantity: args[1].clone(),
        unit: args.get(2).cloned(),
        category: args.get(3).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_command {
        use super::*;

        #[test]
        fn parses_basic_mutations() {
            let cmd = parse_command("add milk 2").unwrap();
            assert_eq!(
                cmd,
                Command::Add(MutationArgs {
                    item_key: "milk".to_string(),
                    quantity: "2".to_string(),
                    unit: None,
                    category: None
                })
            );

            let cmd = parse_command("subtract eggs 12").unwrap();
            assert_eq!(
                cmd,
                Command::Subtract(MutationArgs {
                    item_key: "eggs".to_string(),
                    quantity: "12".to_string(),
                    unit: None,
                    category: None
                })
            );
        }

        #[test]
        fn handles_quoted_strings() {
            let cmd = parse_command("add \"organic milk\" 2").unwrap();
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
            let cmd = parse_command("set bread 2 loaves bakery").unwrap();
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
            let cmd = parse_command("delete milk").unwrap();
            assert_eq!(
                cmd,
                Command::Delete {
                    item_key: "milk".to_string()
                }
            );

            let cmd = parse_command("query \".*dairy.*\"").unwrap();
            assert_eq!(
                cmd,
                Command::Query {
                    filter: Some(".*dairy.*".to_string())
                }
            );

            let cmd = parse_command("query").unwrap();
            assert_eq!(cmd, Command::Query { filter: None });
        }

        #[test]
        fn is_case_insensitive_and_supports_aliases() {
            assert!(parse_command("ADD milk 1").is_ok());
            assert_eq!(
                parse_command("rm milk").unwrap(),
                Command::Delete {
                    item_key: "milk".to_string()
                }
            );
            assert_eq!(parse_command("quit").unwrap(), Command::Exit);
        }

        #[test]
        fn rejects_insufficient_arguments() {
            let res = parse_command("add milk");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("Usage"));
        }

        #[test]
        fn rejects_excessive_arguments() {
            // Delete with 2 args
            let res = parse_command("delete milk eggs");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("Too many"));

            // Query with 2 args
            let res = parse_command("query milk eggs");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("Too many"));

            // Add with 5 args
            let res = parse_command("add milk 2 gallons dairy extra");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("Too many"));
        }

        #[test]
        fn handles_malformed_input() {
            let res = parse_command("add \"unclosed quote");
            assert!(res.is_err());
            assert!(res.unwrap_err().to_string().contains("quoting"));
        }
    }
}
