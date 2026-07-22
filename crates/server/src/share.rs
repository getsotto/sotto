//! Share links - one-time / expiring links for sending a single secret to a non-user.
//!
//! Zero-knowledge: the server stores ciphertext (`enc_blob`) + metadata only; the decryption key
//! lives in the URL fragment and never reaches the server. Creation/revocation are session-gated;
//! fetching is public (the recipient has no account) and burns the view atomically.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;

/// Cap on the shared ciphertext blob.
const MAX_BLOB: usize = 64 * 1024;
/// Cap on the optional passphrase salt.
const MAX_SALT: usize = 64;
/// Cap on `max_views` for a single link.
const MAX_VIEWS: i32 = 100;
/// Cap on a link's lifetime (30 days), in seconds.
const MAX_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shares", post(create_share))
        .route("/shares/{token}", get(fetch_share).delete(revoke_share))
}

#[derive(Deserialize)]
struct CreateShare {
    /// Ciphertext (base64) - the sealed secret. Opaque to the server.
    enc_blob: String,
    /// How many times the link may be fetched before it burns.
    max_views: i32,
    /// Optional lifetime in seconds; absent means no expiry.
    #[serde(default)]
    ttl_seconds: Option<i64>,
    /// Optional Argon2 salt (base64) for a passphrase-protected link.
    #[serde(default)]
    passphrase_salt: Option<String>,
}

#[derive(Serialize)]
struct CreatedShare {
    token: String,
    /// Expiry as an ISO timestamp string, or null if the link never expires.
    expires_at: Option<String>,
}

/// `POST /shares` - create a share link (session required). Returns the public token.
async fn create_share(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateShare>,
) -> Result<(StatusCode, Json<CreatedShare>)> {
    let enc_blob = encoding::decode(&body.enc_blob, "enc_blob", MAX_BLOB)?;
    let passphrase_salt = body
        .passphrase_salt
        .as_deref()
        .map(|s| encoding::decode(s, "passphrase_salt", MAX_SALT))
        .transpose()?;
    validate(body.max_views, body.ttl_seconds)?;

    let token = random_token();
    let row: (String, Option<String>) = sqlx::query_as(
        "INSERT INTO share_links (id, token, enc_blob, passphrase_salt, created_by, max_views, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, \
           CASE WHEN $7::bigint IS NULL THEN NULL ELSE now() + ($7::bigint * interval '1 second') END) \
         RETURNING token, expires_at::text",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&token)
    .bind(&enc_blob)
    .bind(&passphrase_salt)
    .bind(&user.user_id)
    .bind(body.max_views)
    .bind(body.ttl_seconds)
    .fetch_one(&state.pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedShare {
            token: row.0,
            expires_at: row.1,
        }),
    ))
}

#[derive(Serialize)]
struct FetchedShare {
    enc_blob: String,
    passphrase_salt: Option<String>,
}

/// `GET /shares/:token` - fetch the ciphertext (public). Atomically claims a view; once the link is
/// revoked, expired, or exhausted it 404s - uniformly, so the response is no existence oracle.
async fn fetch_share(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<FetchedShare>> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
        "UPDATE share_links SET view_count = view_count + 1 \
         WHERE token = $1 AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > now()) \
           AND view_count < max_views \
         RETURNING enc_blob, passphrase_salt",
    )
    .bind(&token)
    .fetch_optional(&state.pool)
    .await?;

    let (enc_blob, passphrase_salt) =
        row.ok_or_else(|| Error::NotFound("share not found".into()))?;
    Ok(Json(FetchedShare {
        enc_blob: encoding::encode(&enc_blob),
        passphrase_salt: passphrase_salt.map(|s| encoding::encode(&s)),
    }))
}

/// `DELETE /shares/:token` - revoke a link (owner only). 404 if it isn't yours or doesn't exist.
async fn revoke_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<StatusCode> {
    let revoked: Option<String> = sqlx::query_scalar(
        "UPDATE share_links SET revoked_at = now() \
         WHERE token = $1 AND created_by = $2 AND revoked_at IS NULL RETURNING id",
    )
    .bind(&token)
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;

    revoked
        .map(|_| StatusCode::NO_CONTENT)
        .ok_or_else(|| Error::NotFound("share not found".into()))
}

fn validate(max_views: i32, ttl_seconds: Option<i64>) -> Result<()> {
    if !(1..=MAX_VIEWS).contains(&max_views) {
        return Err(Error::BadRequest(format!(
            "max_views must be between 1 and {MAX_VIEWS}"
        )));
    }
    if let Some(ttl) = ttl_seconds {
        if !(1..=MAX_TTL_SECONDS).contains(&ttl) {
            return Err(Error::BadRequest(format!(
                "ttl_seconds must be between 1 and {MAX_TTL_SECONDS}"
            )));
        }
    }
    Ok(())
}

/// A 128-bit random, hex-encoded public link token.
fn random_token() -> String {
    let mut raw = [0u8; 16];
    dryoc::rng::copy_randombytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}
