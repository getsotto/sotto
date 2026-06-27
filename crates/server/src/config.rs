//! Server configuration, read from the environment.

use crate::error::{Error, Result};

/// Default address the server binds to when `SOTTO_BIND` is unset.
const DEFAULT_BIND: &str = "127.0.0.1:8080";

#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Address to bind the HTTP listener to.
    pub bind_addr: String,
}

impl Config {
    /// Load configuration from `DATABASE_URL` (required) and `SOTTO_BIND` (optional).
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| Error::Config("DATABASE_URL is not set".into()))?;
        let bind_addr = std::env::var("SOTTO_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        Ok(Self {
            database_url,
            bind_addr,
        })
    }
}
