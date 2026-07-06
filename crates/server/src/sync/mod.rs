//! Zero-knowledge sync API: projects, environments, and the secret snapshot/batch hot path.
//!
//! The server stores opaque ciphertext (`enc_*`) plus structural metadata, and enforces ownership
//! on every request (defense in depth — cryptography is not access control). Sync is full-snapshot
//! pull keyed by a monotonic per-environment `revision` (the ETag, and the anti-rollback signal),
//! with atomic batch writes guarded by optimistic concurrency on that revision.

pub(crate) mod access;
pub mod environments;
pub mod grants;
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

/// Reject empty, oversize, or unsafely-charactered ids.
///
/// The charset is restricted to `[A-Za-z0-9_-]` (a superset of the UUIDs clients actually mint).
/// This is a **crypto-relevant invariant**, not mere hygiene: the client binds every secret to its
/// location with an AAD of the form `…|env={env_id}|secret={secret_id}|ver=…` (see
/// [`sotto_core::vault`]). Because `|` and `=` are the AAD's delimiters, allowing them inside an id
/// would make that encoding ambiguous — two different `(env, secret)` pairs could serialize to the
/// same AAD string, weakening the substitution/relocation binding. Forbidding delimiter characters
/// here keeps the AAD unambiguous. Keep the two in sync.
pub(crate) fn validate_id(id: &str, field: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID {
        return Err(Error::BadRequest(format!(
            "{field} must be between 1 and {MAX_ID} characters"
        )));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::BadRequest(format!(
            "{field} may contain only letters, digits, '-', and '_'"
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
        .merge(grants::router())
}

#[cfg(test)]
mod tests {
    use super::{validate_id, MAX_ID};

    #[test]
    fn accepts_uuids_and_safe_slugs() {
        assert!(validate_id("550e8400-e29b-41d4-a716-446655440000", "id").is_ok());
        assert!(validate_id("org-create-o", "id").is_ok());
        assert!(validate_id("sub_test_status", "id").is_ok());
        assert!(validate_id("A1", "id").is_ok());
    }

    #[test]
    fn rejects_empty_and_oversize() {
        assert!(validate_id("", "id").is_err());
        assert!(validate_id(&"a".repeat(MAX_ID + 1), "id").is_err());
    }

    #[test]
    fn rejects_aad_delimiters_and_other_unsafe_chars() {
        // The two that would break the AAD encoding directly.
        assert!(validate_id("env=prod", "id").is_err());
        assert!(validate_id("a|b", "id").is_err());
        // Anything else outside the charset is out too.
        assert!(validate_id("has space", "id").is_err());
        assert!(validate_id("a/b", "id").is_err());
        assert!(validate_id("a.b", "id").is_err());
        assert!(validate_id("dropπ", "id").is_err());
    }
}
