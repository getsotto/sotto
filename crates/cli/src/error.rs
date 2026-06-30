//! CLI error type with documented exit codes (see CLI spec §8).

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// No `sotto.toml` in this directory or any parent.
    #[error(
        "no {} found in {} or any parent directory",
        crate::config::CONFIG_FILE,
        .0.parent().unwrap_or_else(|| std::path::Path::new(".")).display()
    )]
    NoConfig(PathBuf),

    /// Failed to parse the config file.
    #[error("config error: {0}")]
    Config(String),

    /// A project/environment/secret was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The store is locked; the user must unlock first.
    #[error("locked — run `sotto unlock` first")]
    Locked,

    /// A storage-layer (SQLite) error.
    #[error("storage error: {0}")]
    Store(String),

    /// An I/O error.
    #[error("i/o error: {0}")]
    Io(String),

    /// A cryptographic operation failed (e.g. wrong password, tampered data).
    #[error("cryptographic operation failed")]
    Crypto,

    /// A concurrent-modification conflict.
    #[error("conflict: {0}")]
    Conflict(String),

    /// `init` was run but an identity already exists.
    #[error("already initialized — an identity already exists")]
    AlreadyInitialized,

    /// No identity has been set up yet.
    #[error("no identity — run `sotto init` first")]
    NoIdentity,

    /// The OS keychain could not be read or written.
    #[error("keychain error: {0}")]
    Keychain(String),

    /// A network/transport failure talking to the server.
    #[error("network error: {0}")]
    Network(String),

    /// The server returned an error response.
    #[error("server error: {0}")]
    Server(String),

    /// Invalid user input (bad arguments, mismatched passwords, or unsafe output).
    #[error("{0}")]
    Input(String),
}

impl Error {
    /// The process exit code for this error (CLI spec §8).
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::NotFound(_) | Error::NoConfig(_) => 3,
            Error::Locked | Error::Crypto | Error::NoIdentity => 4,
            Error::Store(_) | Error::Io(_) | Error::Keychain(_) => 5,
            Error::Network(_) | Error::Server(_) => 5,
            Error::Conflict(_) => 6,
            Error::Config(_) | Error::AlreadyInitialized | Error::Input(_) => 1,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Store(e.to_string())
    }
}

impl From<sotto_core::Error> for Error {
    fn from(_: sotto_core::Error) -> Self {
        // Stay opaque — never leak why an authenticated decryption failed.
        Error::Crypto
    }
}
