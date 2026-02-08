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
}

pub type Result<T> = std::result::Result<T, TitanError>;
