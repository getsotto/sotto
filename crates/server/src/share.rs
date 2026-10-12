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
use sha2::{Digest, Sha256};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::person_eligibility::{self, EligibilityState};
use crate::state::AppState;

/// Cap on the shared ciphertext blob.
const MAX_BLOB: usize = 64 * 1024;
/// Cap on the optional passphrase salt.
const MAX_SALT: usize = 64;
/// Cap on `max_views` for a single link.
const MAX_VIEWS: i32 = 100;
/// Cap on a link's lifetime (30 days), in seconds.
const MAX_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Free hosted accounts may keep three active links at once.
const FREE_ACTIVE_LIMIT: i64 = 3;
/// Free links burn after one successful public fetch and expire after seven days.
const FREE_MAX_VIEWS: i32 = 1;
const FREE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Creation attempts are bounded independently of the active-link allowance.
const RATE_WINDOW_SECONDS: i64 = 60;
const RATE_LIMIT: i32 = 10;
const MAX_IDEMPOTENCY_KEY: usize = 128;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shares", post(create_share).get(list_shares))
        .route("/shares/{token}", get(fetch_share).delete(revoke_share))
}

#[derive(Deserialize)]
struct CreateShare {
    /// Ciphertext (base64) - the sealed secret. Opaque to the server.
    enc_blob: String,
    /// How many times the link may be fetched before it burns.
    #[serde(default)]
    max_views: Option<i32>,
    /// Omitted, explicit no-expiry, and a concrete lifetime remain distinct until policy checks.
    #[serde(default)]
    ttl_seconds: TtlRequest,
    /// Optional Argon2 salt (base64) for a passphrase-protected link.
    #[serde(default)]
    passphrase_salt: Option<String>,
    /// A caller-owned retry identity. It is scoped to the authenticated creator.
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TtlRequest {
    #[default]
    Omitted,
    ExplicitNoExpiry,
    Value(i64),
}

impl<'de> Deserialize<'de> for TtlRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match Option::<i64>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::ExplicitNoExpiry,
        })
    }
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
    let class = share_class(&state, &user.user_id).await?;
    let enc_blob = encoding::decode(&body.enc_blob, "enc_blob", MAX_BLOB)?;
    let passphrase_salt = body
        .passphrase_salt
        .as_deref()
        .map(|s| encoding::decode(s, "passphrase_salt", MAX_SALT))
        .transpose()?;
    let (max_views, ttl_seconds) = normalize_options(class, body.max_views, body.ttl_seconds)?;
    let idempotency_key = validate_idempotency_key(body.idempotency_key.as_deref())?;
    let request_hash = request_hash(
        class,
        &enc_blob,
        passphrase_salt.as_deref(),
        max_views,
        ttl_seconds,
    );

    let token = random_token();
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&user.user_id)
        .fetch_one(&mut *tx)
        .await?;

    if let Some(key) = idempotency_key.as_deref() {
        let existing: Option<(String, Vec<u8>, Option<String>)> = sqlx::query_as(
            "SELECT token, creation_hash, expires_at::text FROM share_links \
             WHERE created_by = $1 AND creation_key = $2",
        )
        .bind(&user.user_id)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((token, existing_hash, expires_at)) = existing {
            if existing_hash != request_hash {
                return Err(Error::Conflict(
                    "share idempotency key conflicts with a different request".into(),
                ));
            }
            tx.commit().await?;
            return Ok((
                StatusCode::CREATED,
                Json(CreatedShare { token, expires_at }),
            ));
        }
    }

    if class == ShareClass::Free {
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM share_links \
             WHERE created_by = $1 AND share_class = 'free' AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > now()) AND view_count < max_views",
        )
        .bind(&user.user_id)
        .fetch_one(&mut *tx)
        .await?;
        if active >= FREE_ACTIVE_LIMIT {
            return Err(Error::Quota(
                "free accounts may have only 3 active share links".into(),
            ));
        }
    }
    enforce_creation_rate(&mut tx, &user.user_id).await?;

    let row: (String, Option<String>) = sqlx::query_as(
        "INSERT INTO share_links \
         (id, token, enc_blob, passphrase_salt, created_by, max_views, expires_at, share_class, \
          creation_key, creation_hash) \
         VALUES ($1, $2, $3, $4, $5, $6, \
           CASE WHEN $7::bigint IS NULL THEN NULL ELSE now() + ($7::bigint * interval '1 second') END, \
           $8, $9, $10) \
         RETURNING token, expires_at::text",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&token)
    .bind(&enc_blob)
    .bind(&passphrase_salt)
    .bind(&user.user_id)
    .bind(max_views)
    .bind(ttl_seconds)
    .bind(class.as_str())
    .bind(idempotency_key)
    .bind(request_hash)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedShare {
            token: row.0,
            expires_at: row.1,
        }),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShareClass {
    Free,
    Paid,
}

impl ShareClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paid => "paid",
        }
    }
}

async fn share_class(state: &AppState, user_id: &str) -> Result<ShareClass> {
    if state.deployment_mode == crate::config::DeploymentMode::SelfHosted {
        return Ok(ShareClass::Paid);
    }
    let view = person_eligibility::load_view(state, user_id)
        .await
        .map_err(|_| Error::CloudEligibility("share policy is unavailable".into()))?;
    Ok(match view.state {
        EligibilityState::Paid | EligibilityState::RenewalRecovery => ShareClass::Paid,
        EligibilityState::Free
        | EligibilityState::PendingInitialPayment
        | EligibilityState::ExportOnly
        | EligibilityState::Expired => ShareClass::Free,
        EligibilityState::Unavailable => {
            return Err(Error::CloudEligibility(
                "share policy is unavailable".into(),
            ))
        }
    })
}

fn normalize_options(
    class: ShareClass,
    max_views: Option<i32>,
    ttl_seconds: TtlRequest,
) -> Result<(i32, Option<i64>)> {
    match class {
        ShareClass::Paid => {
            let max_views = max_views.unwrap_or(1);
            let ttl_seconds = ttl_seconds.value();
            validate(max_views, ttl_seconds)?;
            Ok((max_views, ttl_seconds))
        }
        ShareClass::Free => {
            if let Some(views) = max_views {
                if views != FREE_MAX_VIEWS {
                    return Err(Error::BadRequest(
                        "free share links allow exactly one view".into(),
                    ));
                }
            }
            let ttl_seconds = match ttl_seconds {
                TtlRequest::Omitted => FREE_TTL_SECONDS,
                TtlRequest::Value(ttl) if ttl == FREE_TTL_SECONDS => FREE_TTL_SECONDS,
                TtlRequest::Value(_) | TtlRequest::ExplicitNoExpiry => {
                    return Err(Error::BadRequest(
                        "free share links expire after exactly 7 days".into(),
                    ))
                }
            };
            Ok((FREE_MAX_VIEWS, Some(ttl_seconds)))
        }
    }
}

impl TtlRequest {
    fn value(self) -> Option<i64> {
        match self {
            Self::Omitted | Self::ExplicitNoExpiry => None,
            Self::Value(value) => Some(value),
        }
    }
}

fn validate_idempotency_key(key: Option<&str>) -> Result<Option<String>> {
    let Some(key) = key else { return Ok(None) };
    if key.trim().is_empty() || key.len() > MAX_IDEMPOTENCY_KEY {
        return Err(Error::BadRequest(format!(
            "idempotency_key must be between 1 and {MAX_IDEMPOTENCY_KEY} characters"
        )));
    }
    Ok(Some(key.to_string()))
}

fn request_hash(
    class: ShareClass,
    enc_blob: &[u8],
    passphrase_salt: Option<&[u8]>,
    max_views: i32,
    ttl_seconds: Option<i64>,
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(class.as_str().as_bytes());
    hasher.update((enc_blob.len() as u64).to_be_bytes());
    hasher.update(enc_blob);
    let salt = passphrase_salt.unwrap_or_default();
    hasher.update((salt.len() as u64).to_be_bytes());
    hasher.update(salt);
    hasher.update(max_views.to_be_bytes());
    hasher.update(ttl_seconds.unwrap_or(0).to_be_bytes());
    hasher.finalize().to_vec()
}

async fn enforce_creation_rate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: &str,
) -> Result<()> {
    let attempt_count: i32 = sqlx::query_scalar(
        "INSERT INTO share_creation_rate_limits (user_id, window_started_at, attempt_count) \
         VALUES ($1, now(), 1) \
         ON CONFLICT (user_id) DO UPDATE SET \
           window_started_at = CASE WHEN share_creation_rate_limits.window_started_at \
             <= now() - interval '60 seconds' THEN now() ELSE share_creation_rate_limits.window_started_at END, \
           attempt_count = CASE WHEN share_creation_rate_limits.window_started_at \
             <= now() - interval '60 seconds' THEN 1 ELSE share_creation_rate_limits.attempt_count + 1 END \
         RETURNING attempt_count",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if attempt_count > RATE_LIMIT {
        return Err(Error::RateLimited(format!(
            "too many share link creations; retry after {RATE_WINDOW_SECONDS} seconds"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct ShareSummary {
    token: String,
    share_class: String,
    max_views: i32,
    view_count: i32,
    expires_at: Option<String>,
    revoked_at: Option<String>,
    created_at: String,
}

type ShareSummaryRow = (
    String,
    String,
    i32,
    i32,
    Option<String>,
    Option<String>,
    String,
);

#[derive(Serialize)]
struct ShareList {
    active_free_count: i64,
    links: Vec<ShareSummary>,
}

/// `GET /shares` - list the creator's link metadata without ciphertext or fragment material.
async fn list_shares(State(state): State<AppState>, user: AuthUser) -> Result<Json<ShareList>> {
    let active_free_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM share_links \
         WHERE created_by = $1 AND share_class = 'free' AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > now()) AND view_count < max_views",
    )
    .bind(&user.user_id)
    .fetch_one(&state.pool)
    .await?;
    let rows: Vec<ShareSummaryRow> = sqlx::query_as(
        "SELECT token, share_class, max_views, view_count, expires_at::text, \
                    revoked_at::text, created_at::text \
             FROM share_links WHERE created_by = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(ShareList {
        active_free_count,
        links: rows
            .into_iter()
            .map(
                |(
                    token,
                    share_class,
                    max_views,
                    view_count,
                    expires_at,
                    revoked_at,
                    created_at,
                )| ShareSummary {
                    token,
                    share_class,
                    max_views,
                    view_count,
                    expires_at,
                    revoked_at,
                    created_at,
                },
            )
            .collect(),
    }))
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

#[cfg(test)]
mod tests {
    use super::{normalize_options, CreateShare, ShareClass, TtlRequest, FREE_TTL_SECONDS};

    #[test]
    fn free_options_are_bounded_and_default_to_seven_days() {
        assert_eq!(
            normalize_options(ShareClass::Free, None, TtlRequest::Omitted).unwrap(),
            (1, Some(FREE_TTL_SECONDS))
        );
        assert!(normalize_options(ShareClass::Free, Some(2), TtlRequest::Omitted).is_err());
        assert!(
            normalize_options(ShareClass::Free, Some(1), TtlRequest::ExplicitNoExpiry).is_err()
        );
        assert!(normalize_options(ShareClass::Free, Some(1), TtlRequest::Value(3600)).is_err());
        assert_eq!(
            normalize_options(
                ShareClass::Free,
                Some(1),
                TtlRequest::Value(FREE_TTL_SECONDS)
            )
            .unwrap(),
            (1, Some(FREE_TTL_SECONDS))
        );
    }

    #[test]
    fn paid_options_keep_the_existing_flexibility() {
        assert_eq!(
            normalize_options(ShareClass::Paid, Some(5), TtlRequest::ExplicitNoExpiry).unwrap(),
            (5, None)
        );
        assert_eq!(
            normalize_options(ShareClass::Paid, None, TtlRequest::Value(3600)).unwrap(),
            (1, Some(3600))
        );
    }

    #[test]
    fn idempotency_keys_are_opaque_but_not_blank() {
        assert_eq!(
            super::validate_idempotency_key(Some(" key ")).unwrap(),
            Some(" key ".to_string())
        );
        assert!(super::validate_idempotency_key(Some(" \t ")).is_err());
    }

    #[test]
    fn ttl_request_distinguishes_omitted_null_and_value() {
        let omitted: CreateShare = serde_json::from_str(r#"{"enc_blob":"x"}"#).unwrap();
        let explicit_null: CreateShare =
            serde_json::from_str(r#"{"enc_blob":"x","ttl_seconds":null}"#).unwrap();
        let value: CreateShare =
            serde_json::from_str(r#"{"enc_blob":"x","ttl_seconds":3600}"#).unwrap();
        assert_eq!(omitted.ttl_seconds, TtlRequest::Omitted);
        assert_eq!(explicit_null.ttl_seconds, TtlRequest::ExplicitNoExpiry);
        assert_eq!(value.ttl_seconds, TtlRequest::Value(3600));
    }
}
