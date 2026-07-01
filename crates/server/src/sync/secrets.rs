//! The secret sync hot path: full-snapshot pull (ETag/`If-None-Match`) and atomic batch writes
//! guarded by optimistic concurrency on the environment's monotonic `revision`.

use axum::extract::{Path, State};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::sync::access::env_access;
use crate::sync::{validate_id, MAX_ENC_KEY, MAX_ENC_NAME, MAX_ENC_VALUE};

/// Snapshot row: `(id, enc_name, enc_value, enc_data_key, version, deleted)`.
type SecretRow = (String, Vec<u8>, Vec<u8>, Vec<u8>, i64, bool);

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/environments/{env_id}/secrets",
        get(snapshot).post(write_secrets),
    )
}

#[derive(Serialize)]
struct SecretView {
    id: String,
    enc_name: String,
    enc_value: String,
    enc_data_key: String,
    version: i64,
    /// Tombstone marker: the secret was soft-deleted (its ciphertext is retained at its version).
    deleted: bool,
}

#[derive(Serialize)]
struct Snapshot {
    revision: i64,
    secrets: Vec<SecretView>,
}

#[derive(Deserialize)]
struct BatchRequest {
    /// The revision the client's snapshot was based on; the write applies only if it still holds.
    base_revision: i64,
    changes: Vec<Change>,
}

#[derive(Deserialize)]
struct Change {
    id: String,
    op: ChangeOp,
    /// New per-secret version (set only); bound into the AEAD's AAD, so the server preserves it.
    #[serde(default)]
    version: i64,
    #[serde(default)]
    enc_name: Option<String>,
    #[serde(default)]
    enc_value: Option<String>,
    #[serde(default)]
    enc_data_key: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ChangeOp {
    Set,
    Delete,
}

#[derive(Serialize)]
struct BatchResponse {
    revision: i64,
}

/// `GET /environments/{env_id}/secrets` — full snapshot (including tombstones). Returns 304 when
/// the caller's `If-None-Match` already matches the current revision.
async fn snapshot(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    // Any member (or the personal owner) may read the snapshot.
    env_access(&state, &env_id, &user.user_id).await?;
    let revision = env_revision(&state, &env_id).await?;
    let etag = format!("\"{revision}\"");

    if let Some(inm) = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if parse_etag(inm) == Some(revision) {
            return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
        }
    }

    let rows: Vec<SecretRow> = sqlx::query_as(
        "SELECT id, enc_name, enc_value, enc_data_key, version, (deleted_at IS NOT NULL) \
         FROM secrets WHERE env_id = $1 ORDER BY id",
    )
    .bind(&env_id)
    .fetch_all(&state.pool)
    .await?;

    let secrets = rows
        .into_iter()
        .map(|(id, name, value, data_key, version, deleted)| SecretView {
            id,
            enc_name: encoding::encode(&name),
            enc_value: encoding::encode(&value),
            enc_data_key: encoding::encode(&data_key),
            version,
            deleted,
        })
        .collect();

    Ok(([(ETAG, etag)], Json(Snapshot { revision, secrets })).into_response())
}

/// `POST /environments/{env_id}/secrets` — apply a batch of changes atomically. Optimistic
/// concurrency: 412 if `base_revision` no longer matches. One batch = one monotonic revision bump.
async fn write_secrets(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    Json(req): Json<BatchRequest>,
) -> Result<Response> {
    if req.changes.is_empty() {
        return Err(Error::BadRequest(
            "batch must contain at least one change".into(),
        ));
    }

    // Authorize before touching the environment; any member (or personal owner) may write secrets.
    env_access(&state, &env_id, &user.user_id).await?;

    let mut tx = state.pool.begin().await?;

    // Lock the environment row so concurrent batches serialize on its revision.
    let current: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1 FOR UPDATE")
            .bind(&env_id)
            .fetch_optional(&mut *tx)
            .await?;
    let current = current.ok_or_else(|| Error::NotFound("environment not found".into()))?;

    if current != req.base_revision {
        return Err(Error::Precondition(
            "base_revision is stale; re-pull the snapshot".into(),
        ));
    }

    for change in &req.changes {
        validate_id(&change.id, "change id")?;
        match change.op {
            ChangeOp::Set => apply_set(&mut tx, &env_id, change).await?,
            ChangeOp::Delete => apply_delete(&mut tx, &env_id, change).await?,
        }
    }

    let new_revision = current + 1;
    sqlx::query("UPDATE environments SET revision = $2 WHERE id = $1")
        .bind(&env_id)
        .bind(new_revision)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let etag = format!("\"{new_revision}\"");
    Ok((
        [(ETAG, etag)],
        Json(BatchResponse {
            revision: new_revision,
        }),
    )
        .into_response())
}

/// Upsert a secret (resurrecting a tombstone) and append a history row.
async fn apply_set(
    tx: &mut Transaction<'_, Postgres>,
    env_id: &str,
    change: &Change,
) -> Result<()> {
    if change.version < 1 {
        return Err(Error::BadRequest("set requires version >= 1".into()));
    }
    let enc_name = decode_required(&change.enc_name, "enc_name", MAX_ENC_NAME)?;
    let enc_value = decode_required(&change.enc_value, "enc_value", MAX_ENC_VALUE)?;
    let enc_data_key = decode_required(&change.enc_data_key, "enc_data_key", MAX_ENC_KEY)?;

    // The `WHERE secrets.env_id = $2` guard means an id that already exists under a *different*
    // environment matches nothing here: 0 rows affected → reject rather than corrupt that secret.
    let res = sqlx::query(
        "INSERT INTO secrets (id, env_id, enc_name, enc_value, enc_data_key, version) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (id) DO UPDATE SET \
           enc_name = EXCLUDED.enc_name, enc_value = EXCLUDED.enc_value, \
           enc_data_key = EXCLUDED.enc_data_key, version = EXCLUDED.version, \
           deleted_at = NULL, updated_at = now() \
         WHERE secrets.env_id = $2",
    )
    .bind(&change.id)
    .bind(env_id)
    .bind(&enc_name)
    .bind(&enc_value)
    .bind(&enc_data_key)
    .bind(change.version)
    .execute(&mut **tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(Error::Conflict(format!(
            "secret id {} belongs to another environment",
            change.id
        )));
    }

    sqlx::query(
        "INSERT INTO secret_versions (id, secret_id, version, enc_name, enc_value, enc_data_key) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&change.id)
    .bind(change.version)
    .bind(&enc_name)
    .bind(&enc_value)
    .bind(&enc_data_key)
    .execute(&mut **tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            Error::Conflict("secret version already exists".into())
        } else {
            e.into()
        }
    })?;
    Ok(())
}

/// Soft-delete a secret. Idempotent and cross-env safe: a no-op when the id is not a secret in this
/// environment. Version and ciphertext are left intact (the AAD binds the version), so the
/// tombstone is just `deleted_at`; the batch's revision bump signals the change to other devices.
async fn apply_delete(
    tx: &mut Transaction<'_, Postgres>,
    env_id: &str,
    change: &Change,
) -> Result<()> {
    sqlx::query(
        "UPDATE secrets SET deleted_at = now(), updated_at = now() WHERE id = $1 AND env_id = $2",
    )
    .bind(&change.id)
    .bind(env_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Look up an environment's current revision. The caller must already be authorized via
/// [`env_access`]; this only reads the revision the ETag is built from.
async fn env_revision(state: &AppState, env_id: &str) -> Result<i64> {
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1")
            .bind(env_id)
            .fetch_optional(&state.pool)
            .await?;
    revision.ok_or_else(|| Error::NotFound("environment not found".into()))
}

fn decode_required(value: &Option<String>, field: &str, max: usize) -> Result<Vec<u8>> {
    let value = value
        .as_deref()
        .ok_or_else(|| Error::BadRequest(format!("set requires {field}")))?;
    encoding::decode(value, field, max)
}

/// Parse an `If-None-Match` value (`"5"`, `W/"5"`, or `5`) into a revision number.
fn parse_etag(value: &str) -> Option<i64> {
    value
        .trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

#[cfg(test)]
mod tests {
    use super::parse_etag;

    #[test]
    fn parses_etag_forms() {
        assert_eq!(parse_etag("\"5\""), Some(5));
        assert_eq!(parse_etag("W/\"7\""), Some(7));
        assert_eq!(parse_etag("12"), Some(12));
        assert_eq!(parse_etag("*"), None);
        assert_eq!(parse_etag("\"abc\""), None);
    }
}
