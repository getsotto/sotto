//! Sotto sync / API server.
//!
//! The server is **zero-knowledge**: it stores ciphertext (secret names/values/data-keys, wrapped
//! vault keys) plus structural metadata, and never sees plaintext or keys.
//!
//! - [`config`] — server configuration from the environment
//! - [`db`] — Postgres connection pool + migrations
//! - [`auth`] — GitHub OAuth login, sessions, and the [`auth::AuthUser`] request extractor
//! - [`account`] — account crypto-material sync (KDF params, public key, sealed private keys, …)
//! - [`state`] — shared application state ([`state::AppState`])
//! - [`error`] — server error type

pub mod account;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod state;
