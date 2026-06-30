//! Server error type.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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

    /// The request lacked a valid session (missing/expired/unknown bearer token).
    #[error("unauthorized")]
    Unauthorized,

    /// The request was malformed (bad/expired login state, non-loopback redirect, …).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The requested resource does not exist (e.g. an uninitialized account).
    #[error("not found: {0}")]
    NotFound(String),

    /// The request conflicts with current state (e.g. re-initializing an account).
    #[error("conflict: {0}")]
    Conflict(String),

    /// A precondition failed (e.g. a stale `base_revision` on a sync write).
    #[error("precondition failed: {0}")]
    Precondition(String),

    /// An optional feature (e.g. OAuth) is not configured on this server.
    #[error("not configured: {0}")]
    NotConfigured(String),

    /// A call to an upstream identity provider failed.
    #[error("upstream error: {0}")]
    Upstream(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Error::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            Error::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Error::Precondition(m) => (StatusCode::PRECONDITION_FAILED, m.clone()),
            Error::NotConfigured(m) => (StatusCode::SERVICE_UNAVAILABLE, m.clone()),
            Error::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream authentication error".to_string(),
            ),
            // Internal faults: never leak details to the client; log them server-side.
            Error::Db(_) | Error::Config(_) | Error::Migrate(_) | Error::Io(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ),
        };

        if status.is_server_error() {
            eprintln!("server error: {self}");
        }
        (status, message).into_response()
    }
}
