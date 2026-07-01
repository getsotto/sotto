//! Zero-knowledge sync API: projects, environments, and the secret snapshot/batch hot path.
//!
//! The server stores opaque ciphertext (`enc_*`) plus structural metadata, and enforces ownership
//! on every request (defense in depth — cryptography is not access control). Sync is full-snapshot
//! pull keyed by a monotonic per-environment `revision` (the ETag, and the anti-rollback signal),
//! with atomic batch writes guarded by optimistic concurrency on that revision.

pub(crate) mod access;
pub mod environments;
pub mod projects;
pub mod secrets;

use axum::Router;

use crate::error::{Error, Result};
use crate::state::AppState;

/// Max length of a client-supplied id (project / environment / secret). They are UUIDs in practice.
pub(crate) const MAX_ID: usize = 128;
/// Cap on an encrypted name blob.
pub(crate) const MAX_ENC_NAME: usize = 4 * 1024;
/// Cap on an encrypted secret value blob.
pub(crate) const MAX_ENC_VALUE: usize = 64 * 1024;
/// Cap on a wrapped key blob (per-secret data key or per-environment vault key).
pub(crate) const MAX_ENC_KEY: usize = 1024;

/// Reject empty or oversize ids.
pub(crate) fn validate_id(id: &str, field: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID {
        return Err(Error::BadRequest(format!(
            "{field} must be between 1 and {MAX_ID} characters"
        )));
    }
    Ok(())
}

/// All sync routes, to be merged into the top-level router.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(projects::router())
        .merge(environments::router())
        .merge(secrets::router())
}
