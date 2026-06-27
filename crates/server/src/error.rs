//! Server error type.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// A required configuration value was missing or invalid.
    #[error("config error: {0}")]
    Config(String),

    /// A database query or connection error.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// A schema migration failed.
    #[error("migration error: {0}")]
    Migrate(String),

    /// An I/O error (binding the listener, serving, …).
    #[error("i/o error: {0}")]
    Io(String),
}
