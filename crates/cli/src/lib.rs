//! Sotto CLI library — the local, end-to-end-encrypted secret store behind the `sotto` binary.
//!
//! M2 is local-only (no server, sync, or sharing). The store holds ciphertext and mirrors the
//! server schema so M3 sync is additive.
//!
//! - [`config`] — the committed, secret-free `sotto.toml` project config
//! - [`store`] — the local SQLite store of encrypted rows + version history
//! - [`error`] — CLI errors with documented exit codes

pub mod config;
pub mod error;
pub mod store;
