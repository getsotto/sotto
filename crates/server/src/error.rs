//! Server error type.

use axum::http::{header::HeaderName, HeaderValue, StatusCode};
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

    /// An internal invariant failed without an upstream or database error.
    #[error("internal error: {0}")]
    Internal(String),

    /// A schema migration failed.
    #[error("migration error: {0}")]
    Migrate(String),

    /// An I/O error (binding the listener, serving, …).
    #[error("i/o error: {0}")]
    Io(String),

    /// The request lacked a valid session (missing/expired/unknown bearer token).
    #[error("unauthorised")]
    Unauthorized,

    /// The caller is authenticated but lacks the role/permission for this action.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A plan quota or Team-feature gate blocked the request.
    #[error("quota: {0}")]
    Quota(String),

    /// A hosted Cloud action requires an eligible person-level account.
    #[error("cloud eligibility: {0}")]
    CloudEligibility(String),

    /// The request was malformed (bad/expired login state, non-loopback redirect, …).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The requested resource does not exist (e.g. an uninitialised account).
    #[error("not found: {0}")]
    NotFound(String),

    /// The request conflicts with current state (e.g. re-initialising an account).
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

    /// The caller must retry later because the service is rate limited.
    #[error("rate limited: {0}")]
    RateLimited(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorised".to_string()),
            Error::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            Error::Quota(m) | Error::CloudEligibility(m) => {
                (StatusCode::PAYMENT_REQUIRED, m.clone())
            }
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Error::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            Error::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Error::Precondition(m) => (StatusCode::PRECONDITION_FAILED, m.clone()),
            Error::NotConfigured(m) => (StatusCode::SERVICE_UNAVAILABLE, m.clone()),
            Error::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream authentication error".to_string(),
            ),
            Error::RateLimited(m) => (StatusCode::TOO_MANY_REQUESTS, m.clone()),
            // Internal faults: never leak details to the client; log them server-side.
            Error::Db(_)
            | Error::Internal(_)
            | Error::Config(_)
            | Error::Migrate(_)
            | Error::Io(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ),
        };

        if status.is_server_error() {
            eprintln!("server error: {self}");
        }
        let code = self.code();
        let mut response = (status, message).into_response();
        if matches!(self, Error::RateLimited(_)) {
            response.headers_mut().insert(
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("60"),
            );
        }
        response.headers_mut().insert(
            HeaderName::from_static("x-sotto-error-code"),
            HeaderValue::from_static(code),
        );
        response
    }
}

impl Error {
    /// Stable machine-readable error taxonomy. The response body remains the legacy plain text
    /// form so older clients can continue to render it unchanged.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::Quota(_) => "quota",
            Self::CloudEligibility(_) => "cloud_eligibility_required",
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Precondition(_) => "precondition_failed",
            Self::NotConfigured(_) => "unavailable",
            Self::RateLimited(_) => "rate_limited",
            Self::Upstream(_) => "upstream_error",
            Self::Db(_) | Self::Internal(_) | Self::Config(_) | Self::Migrate(_) | Self::Io(_) => {
                "internal_error"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use axum::response::IntoResponse;

    #[test]
    fn error_codes_are_stable_and_old_clients_keep_plain_messages() {
        assert_eq!(Error::Unauthorized.code(), "unauthorized");
        assert_eq!(Error::Quota("limit".into()).code(), "quota");
        assert_eq!(
            Error::CloudEligibility("upgrade".into()).code(),
            "cloud_eligibility_required"
        );
        assert_eq!(Error::RateLimited("retry".into()).code(), "rate_limited");
        assert_eq!(Error::NotConfigured("offline".into()).code(), "unavailable");
    }

    #[test]
    fn rate_limits_include_a_retry_hint() {
        let response = Error::RateLimited("try later".into()).into_response();
        assert_eq!(response.headers()["retry-after"], "60");
    }
}
