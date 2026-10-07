use thiserror::Error;

#[derive(Error, Debug)]
pub enum TitanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Page not found: {0}")]
    PageNotFound(u64),
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    #[error("Lock error")]
    LockError,
    #[error("Row too large: {0} bytes")]
    RowTooLarge(usize),
    #[error("Corrupt page {0}: {1}")]
    CorruptPage(u64, String),
    #[error("Unsupported SQL: {0}")]
    UnsupportedSql(String),
    #[error("Transaction conflict")]
    TxConflict,
    #[error("Transaction aborted")]
    TxAborted,
    #[error("WAL corrupt: {0}")]
    WalCorrupt(String),
    #[error("Catalog missing: {0}")]
    CatalogMissing(String),
    #[error("Invalid SQL: {0}")]
    InvalidSql(String),
    #[error("Table exists: {0}")]
    TableExists(String),
    #[error("Table not found: {0}")]
    TableNotFound(String),
}

impl TitanError {
    /// WS-safe code string — no internal details leaked.
    pub fn user_safe_code(&self) -> &'static str {
        match self {
            TitanError::Io(_) => "IO_ERROR",
            TitanError::PageNotFound(_) => "PAGE_NOT_FOUND",
            TitanError::Serialization(_) => "SERIALIZATION_ERROR",
            TitanError::LockError => "LOCK_ERROR",
            TitanError::RowTooLarge(_) => "ROW_TOO_LARGE",
            TitanError::CorruptPage(_, _) => "CORRUPT_PAGE",
            TitanError::UnsupportedSql(_) => "UNSUPPORTED_SQL",
            TitanError::TxConflict => "TX_CONFLICT",
            TitanError::TxAborted => "TX_ABORTED",
            TitanError::WalCorrupt(_) => "WAL_CORRUPT",
            TitanError::CatalogMissing(_) => "CATALOG_MISSING",
            TitanError::InvalidSql(_) => "INVALID_SQL",
            TitanError::TableExists(_) => "TABLE_EXISTS",
            TitanError::TableNotFound(_) => "TABLE_NOT_FOUND",
        }
    }

    /// User-safe message (safe to send over WS). None = internal, use generic fallback.
    pub fn user_message(&self) -> Option<String> {
        match self {
            TitanError::InvalidSql(m) | TitanError::UnsupportedSql(m) => Some(m.clone()),
            TitanError::TableExists(t) => Some(format!("Table {t} already exists")),
            TitanError::TableNotFound(t) => Some(format!("Table {t} not found")),
            TitanError::CatalogMissing(m) | TitanError::WalCorrupt(m) => Some(m.clone()),
            TitanError::RowTooLarge(_) => Some("row too large".to_string()),
            TitanError::TxConflict => Some("transaction conflict, retry".to_string()),
            TitanError::TxAborted => Some("transaction aborted".to_string()),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, TitanError>;
