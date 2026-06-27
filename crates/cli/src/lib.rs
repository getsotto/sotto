//! Sotto CLI library — the local, end-to-end-encrypted secret store behind the `sotto` binary.
//!
//! M2 is local-only (no server, sync, or sharing). The store holds ciphertext and mirrors the
//! server schema so M3 sync is additive.
//!
//! - [`config`] — the committed, secret-free `sotto.toml` project config
//! - [`store`] — the local SQLite store of encrypted rows + version history
//! - [`vault`] — crypto orchestration (sotto-core over the store): the key hierarchy + E2EE secrets
//! - [`keychain`] — OS keychain abstraction (with an in-memory mock for tests)
//! - [`session`] — identity setup, unlock/lock, and the TTL master-key session
//! - [`commands`] — the testable command layer (secret operations) behind the binary
//! - [`paths`] — store / data-directory locations
//! - [`error`] — CLI errors with documented exit codes

pub mod commands;
pub mod config;
pub mod error;
pub mod keychain;
pub mod paths;
pub mod session;
pub mod store;
pub mod vault;
