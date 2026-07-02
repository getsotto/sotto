//! Account crypto-material sync.
//!
//! After OAuth login (PR2), a device fetches the non-secret KDF inputs and *ciphertext* key
//! material it needs to reconstruct the vault: the KDF parameters (incl. salt), the shareable
//! public key, the private keys sealed under the master key, and the Emergency Kit recovery blob.
//!
//! Every field is **server-opaque** — base64 over JSON, stored as `BYTEA`, returned verbatim. The
//! server never sees a master key, password, or plaintext private key, so the zero-knowledge
//! guarantee holds.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::state::AppState;

/// Row shape for the four account blobs: `(public_key, enc_private_keys, kdf_params, recovery_blob)`.
type AccountRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

/// X25519 public key length, in bytes.
const PUBLIC_KEY_LEN: usize = 32;
/// Per-field cap on opaque key material (well above realistic sizes; axum also bounds the body).
const MAX_BLOB: usize = 16 * 1024;
/// Longest base64 input that can decode within [`MAX_BLOB`] (base64 is 4 chars per 3 bytes). Used
/// to reject oversize fields before allocating/decoding them.
const MAX_ENCODED: usize = MAX_BLOB.div_ceil(3) * 4;

/// All four routes share the `/account` path.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/account", get(get_account).put(put_account))
        .route("/account/reset", axum::routing::post(reset_account))
}

/// The account bundle as it travels over the wire (all fields base64-encoded).
#[derive(Deserialize, Serialize)]
pub struct AccountBundle {
    /// Shareable X25519 public key (exactly 32 bytes once decoded).
    pub public_key: String,
    /// X25519 private keys sealed under the master key.
    pub enc_private_keys: String,
    /// Opaque KDF parameters (Argon2 params + salt), serialized by the client.
    pub kdf_params: String,
    /// Emergency Kit recovery material.
    pub recovery_blob: String,
}

/// `PUT /account` — initialize the account's crypto material. Create-only: a second call on an
/// already-initialized account returns 409 (re-keying is a dedicated future operation).
async fn put_account(
    State(state): State<AppState>,
    user: AuthUser,
    Json(bundle): Json<AccountBundle>,
) -> Result<StatusCode> {
    let public_key = decode(&bundle.public_key, "public_key")?;
    if public_key.len() != PUBLIC_KEY_LEN {
        return Err(Error::BadRequest(format!(
            "public_key must decode to {PUBLIC_KEY_LEN} bytes"
        )));
    }
    let enc_private_keys = decode(&bundle.enc_private_keys, "enc_private_keys")?;
    let kdf_params = decode(&bundle.kdf_params, "kdf_params")?;
    let recovery_blob = decode(&bundle.recovery_blob, "recovery_blob")?;

    // Create-only: matches a row solely when the account is uninitialized. AuthUser guarantees the
    // user row exists, so "no row updated" can only mean "already initialized".
    let initialized: Option<String> = sqlx::query_scalar(
        "UPDATE users \
         SET public_key = $2, enc_private_keys = $3, kdf_params = $4, recovery_blob = $5 \
         WHERE id = $1 AND public_key IS NULL \
         RETURNING id",
    )
    .bind(&user.user_id)
    .bind(&public_key)
    .bind(&enc_private_keys)
    .bind(&kdf_params)
    .bind(&recovery_blob)
    .fetch_optional(&state.pool)
    .await?;

    match initialized {
        Some(_) => Ok(StatusCode::CREATED),
        None => Err(Error::Conflict("account already initialized".into())),
    }
}

/// `POST /account/reset` — replace the account's crypto material with freshly generated keys (the
/// recovery path for a user who lost their Emergency Kit but can still log in). Everything sealed
/// to the OLD keys becomes permanently unreadable — that is the zero-knowledge deal — so the user's
/// now-dead environment grants are deleted in the same transaction: a clean "not granted" beats a
/// confusing decrypt failure, and org admins simply re-grant the new key. 404 until the account is
/// initialized (first-time setup goes through `PUT /account`).
async fn reset_account(
    State(state): State<AppState>,
    user: AuthUser,
    Json(bundle): Json<AccountBundle>,
) -> Result<StatusCode> {
    let public_key = decode(&bundle.public_key, "public_key")?;
    if public_key.len() != PUBLIC_KEY_LEN {
        return Err(Error::BadRequest(format!(
            "public_key must decode to {PUBLIC_KEY_LEN} bytes"
        )));
    }
    let enc_private_keys = decode(&bundle.enc_private_keys, "enc_private_keys")?;
    let kdf_params = decode(&bundle.kdf_params, "kdf_params")?;
    let recovery_blob = decode(&bundle.recovery_blob, "recovery_blob")?;

    let mut tx = state.pool.begin().await?;
    let reset: Option<String> = sqlx::query_scalar(
        "UPDATE users \
         SET public_key = $2, enc_private_keys = $3, kdf_params = $4, recovery_blob = $5 \
         WHERE id = $1 AND public_key IS NOT NULL \
         RETURNING id",
    )
    .bind(&user.user_id)
    .bind(&public_key)
    .bind(&enc_private_keys)
    .bind(&kdf_params)
    .bind(&recovery_blob)
    .fetch_optional(&mut *tx)
    .await?;
    if reset.is_none() {
        return Err(Error::NotFound(
            "account is not initialized; use PUT /account".into(),
        ));
    }
    sqlx::query("DELETE FROM environment_grants WHERE user_id = $1")
        .bind(&user.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// `GET /account` — download the owner's crypto material. 404 until the account is initialized.
async fn get_account(State(state): State<AppState>, user: AuthUser) -> Result<Json<AccountBundle>> {
    // All four columns are written together by `put_account`, so when `public_key` is non-null the
    // rest are too — selecting them as non-optional bytes is sound.
    let row: Option<AccountRow> = sqlx::query_as(
        "SELECT public_key, enc_private_keys, kdf_params, recovery_blob \
         FROM users WHERE id = $1 AND public_key IS NOT NULL",
    )
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;

    let (public_key, enc_private_keys, kdf_params, recovery_blob) =
        row.ok_or_else(|| Error::NotFound("account is not initialized".into()))?;

    Ok(Json(AccountBundle {
        public_key: STANDARD.encode(public_key),
        enc_private_keys: STANDARD.encode(enc_private_keys),
        kdf_params: STANDARD.encode(kdf_params),
        recovery_blob: STANDARD.encode(recovery_blob),
    }))
}

/// Decode a base64 field, rejecting malformed input or anything over [`MAX_BLOB`].
fn decode(value: &str, field: &str) -> Result<Vec<u8>> {
    // Bound the work up front: reject oversize input before decoding so a large field can't force a
    // big allocation just to be rejected afterward.
    if value.len() > MAX_ENCODED {
        return Err(Error::BadRequest(format!(
            "{field} exceeds {MAX_BLOB} bytes"
        )));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| Error::BadRequest(format!("{field} is not valid base64")))?;
    if bytes.len() > MAX_BLOB {
        return Err(Error::BadRequest(format!(
            "{field} exceeds {MAX_BLOB} bytes"
        )));
    }
    Ok(bytes)
}
