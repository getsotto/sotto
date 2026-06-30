//! Talking to the sync server: configuration, the HTTP client, and login.
//!
//! PR5b-i (connectivity): server-URL config, the [`SyncApi`] client over HTTP, and the loopback
//! `login` flow. The sync engine (push/pull reconciliation) lands in PR5b-ii, targeting [`SyncApi`].

pub mod api;
pub mod auth;
pub mod config;
pub mod http;
pub mod sync;

pub use api::SyncApi;
pub use http::HttpClient;
