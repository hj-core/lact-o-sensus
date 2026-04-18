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
