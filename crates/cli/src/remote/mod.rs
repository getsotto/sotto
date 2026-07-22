//! Talking to the sync server.
//!
//! - [`config`] - server-URL configuration; [`auth`] - the loopback OAuth `login` flow
//! - [`api`] - the [`SyncApi`] trait + wire types; [`http`] - its reqwest implementation
//! - [`sync`] - the push/pull reconciliation engine
//! - [`team`] - organisations, invites, environment sharing, rotation, and removal
//! - [`machine`] - the `SOTTO_TOKEN` (CI) mode; [`share`] - one-time share links

pub mod api;
pub mod auth;
pub mod config;
pub mod http;
pub mod machine;
pub mod share;
pub mod sync;
pub mod team;

pub use api::SyncApi;
pub use http::HttpClient;
