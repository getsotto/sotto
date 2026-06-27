//! Sotto sync / API server.
//!
//! The server is **zero-knowledge**: it stores ciphertext (secret names/values/data-keys, wrapped
//! vault keys) plus structural metadata, and never sees plaintext or keys. M3 PR1 is the
//! foundation — config, the Postgres pool, and migrations; the sync endpoints land in later PRs.
//!
//! - [`config`] — server configuration from the environment
//! - [`db`] — Postgres connection pool + migrations
//! - [`error`] — server error type

pub mod config;
pub mod db;
pub mod error;
