pub mod executor;

use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionResult {
    Message(String),
    ResultSet {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

impl std::fmt::Display for ExecutionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionResult::Message(msg) => write!(f, "{}", msg),
            ExecutionResult::ResultSet { columns, rows } => {
                writeln!(f, "{}", columns.join(" | "))?;
                writeln!(f, "{}", "-".repeat(columns.len() * 10))?;
                for row in rows {
                    writeln!(f, "{}", row.join(" | "))?;
                }
                Ok(())
            }
        }
    }
}