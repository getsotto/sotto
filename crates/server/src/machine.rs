//! Machine identities: per-environment tokens for CI / service access.
//!
//! A machine token binds exactly one environment and carries its own grant (the env vault key
//! sealed to the machine's X25519 public key, both generated client-side). The server stores only
//! the API token's hash, the public key, and the sealed grant - it authenticates machines but can
//! never decrypt. The raw token (`smt_<hex>`) is returned to the creator exactly once; the CLI
//! combines it with the machine's private key into the `SOTTO_TOKEN` string.
//!
//! Machines get a deliberately tiny, read-only surface: `GET /machine/grant` and
//! `GET /machine/secrets`, both authenticated by the machine token alone. They cannot reach any
//! other endpoint, and user sessions cannot reach these (different token namespace).

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::session;
use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::sync::access::env_access;
use crate::sync::{validate_id, MAX_ENC_KEY};

/// Bytes of randomness in a machine token (256 bits).
const TOKEN_BYTES: usize = 32;
/// Machine-token prefix - a distinct namespace from `st_` sessions, so neither kind of bearer
/// works where the other is expected.
const TOKEN_PREFIX: &str = "smt_";
/// X25519 public key length, in bytes.
const PUBLIC_KEY_LEN: usize = 32;
/// Cap on a token's human label.
const MAX_NAME: usize = 128;
/// Lifetime of a new token, in days. Long enough to cover a quarterly release cycle and act on
/// the expiry warning, short enough that a forgotten token dies within a quarter.
const DEFAULT_LIFETIME_DAYS: i64 = 90;
/// Longest lifetime a creator may choose, in days. There is no "never": a token that outlives
/// everyone's memory of it is the problem expiry exists to solve.
const MAX_LIFETIME_DAYS: i64 = 365;

/// SQL: the token can still authenticate, meaning not revoked and not yet past its end date.
/// Every query that means "active" splices in this one fragment (auth, the grant re-read, the
/// listing, rotation coverage), so they cannot drift apart: a listing that kept an expired token
/// would make rotation re-seal a dead grant, and one that dropped a live token would strand its
/// CI on the old key. Columns are unqualified, so a query using it must not join another table
/// that has a `revoked_at` or `expires_at` column.
macro_rules! active_token_sql {
    () => {
        "revoked_at IS NULL AND expires_at > now()"
    };
}
pub(crate) use active_token_sql;

/// SQL: a token's end date as `(expires_at, expires_in_days)`. The timestamp is UTC in the audit
/// log's format; the days left are whole days rounded down, computed here so that no client does
/// date arithmetic against its own clock (a CI runner's is the least trustworthy one involved).
macro_rules! expiry_columns_sql {
    () => {
        "to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
         floor(extract(epoch FROM expires_at - now()) / 86400)::bigint"
    };
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/environments/{env_id}/tokens",
            get(list_tokens).post(create_token),
        )
        .route(
            "/environments/{env_id}/tokens/{token_id}",
            axum::routing::delete(revoke_token),
        )
        .route("/machine/grant", get(machine_grant))
        .route("/machine/secrets", get(machine_secrets))
}

// --- management (user-session-authed, admin+/owner) ---------------------------------------------

#[derive(Deserialize)]
struct CreateToken {
    /// Human label for listings ("github-actions").
    name: String,
    /// The machine's X25519 public key (generated client-side; base64).
    public_key: String,
    /// The env vault key sealed to that public key (base64) - the machine's grant.
    enc_vault_key: String,
    /// Lifetime in days, 1 to `MAX_LIFETIME_DAYS`; `DEFAULT_LIFETIME_DAYS` when omitted, so older
    /// clients that never send it still get tokens that expire.
    #[serde(default)]
    expires_in_days: Option<i64>,
}

#[derive(Serialize)]
struct CreatedToken {
    token_id: String,
    /// The raw API token, shown exactly once. Only its hash is stored.
    token: String,
    /// When the token stops authenticating (UTC, RFC 3339).
    expires_at: String,
}

#[derive(Serialize)]
struct TokenView {
    token_id: String,
    name: String,
    /// The machine's public key (base64) - rotation re-seals the new vault key to this.
    public_key: String,
    /// The user who created the token, if still known (`NULL` once their account is gone).
    created_by: Option<String>,
    /// When the token stops authenticating (UTC, RFC 3339).
    expires_at: String,
    /// Whole days until then, rounded down.
    expires_in_days: i64,
}

/// Listing row: `(id, name, public_key, created_by, expires_at, expires_in_days)`.
type TokenRow = (String, String, Vec<u8>, Option<String>, String, i64);

#[derive(Deserialize, Default)]
struct TokenListParams {
    /// Only tokens created by this user (member-removal review, hand remediation).
    #[serde(default)]
    created_by: Option<String>,
}

/// `POST /environments/{env_id}/tokens` - create a machine token for this environment (admin+ or
/// the personal owner). Returns the raw API token once.
async fn create_token(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    Json(body): Json<CreateToken>,
) -> Result<(StatusCode, Json<CreatedToken>)> {
    if body.name.is_empty() || body.name.len() > MAX_NAME {
        return Err(Error::BadRequest(format!(
            "name must be between 1 and {MAX_NAME} characters"
        )));
    }
    let public_key = encoding::decode(&body.public_key, "public_key", MAX_ENC_KEY)?;
    if public_key.len() != PUBLIC_KEY_LEN {
        return Err(Error::BadRequest(format!(
            "public_key must decode to {PUBLIC_KEY_LEN} bytes"
        )));
    }
    let enc_vault_key = encoding::decode(&body.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?;
    let lifetime_days = body.expires_in_days.unwrap_or(DEFAULT_LIFETIME_DAYS);
    if !(1..=MAX_LIFETIME_DAYS).contains(&lifetime_days) {
        return Err(Error::BadRequest(format!(
            "expires_in_days must be between 1 and {MAX_LIFETIME_DAYS}"
        )));
    }

    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    access.require_manage_structure("must be an admin or owner to create a machine token")?;
    let audit_org = access.org_id().map(str::to_string);

    let token_id = uuid::Uuid::new_v4().to_string();
    let token = generate_token();
    // Token row and its audit event commit together, so a failed audit can't leave one un-logged.
    let mut tx = state.pool.begin().await?;
    access
        .require_manage_structure_tx(
            &mut tx,
            "must be an admin or owner to create a machine token",
        )
        .await?;
    // The end date comes from the database clock, the same one every expiry check reads.
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO machine_tokens \
         (id, env_id, name, token_hash, public_key, enc_vault_key, created_by, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8::int)) \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(&token_id)
    .bind(&env_id)
    .bind(&body.name)
    .bind(session::hash_token(&token))
    .bind(&public_key)
    .bind(&enc_vault_key)
    .bind(&user.user_id)
    .bind(lifetime_days)
    .fetch_one(&mut *tx)
    .await?;
    // Personal environments have no org, hence no audit log to write to.
    if let Some(org) = &audit_org {
        audit::record_tx(
            &mut tx,
            org,
            &user.user_id,
            "token.created",
            audit::Context {
                target: Some(&token_id),
                env_id: Some(&env_id),
                detail: Some(&body.name),
            },
        )
        .await?;
    }
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token_id,
            token,
            expires_at,
        }),
    ))
}

/// `GET /environments/{env_id}/tokens` - the environment's *active* machine tokens (admin+), so
/// revoked and expired ones are left out. A rotation uses these public keys to re-seal every
/// machine's grant. `?created_by=` narrows the listing to one creator's tokens.
async fn list_tokens(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    Query(params): Query<TokenListParams>,
) -> Result<Json<Vec<TokenView>>> {
    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    if !access.can_manage_structure() {
        return Err(Error::Forbidden(
            "must be an admin or owner to list machine tokens".into(),
        ));
    }
    let rows: Vec<TokenRow> = sqlx::query_as(concat!(
        "SELECT id, name, public_key, created_by, ",
        expiry_columns_sql!(),
        " FROM machine_tokens WHERE env_id = $1 AND ",
        active_token_sql!(),
        " AND ($2::text IS NULL OR created_by = $2) ORDER BY id",
    ))
    .bind(&env_id)
    .bind(&params.created_by)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(token_id, name, public_key, created_by, expires_at, expires_in_days)| TokenView {
                    token_id,
                    name,
                    public_key: encoding::encode(&public_key),
                    created_by,
                    expires_at,
                    expires_in_days,
                },
            )
            .collect(),
    ))
}

/// `DELETE /environments/{env_id}/tokens/{token_id}` - revoke a machine token (admin+). Its API
/// access dies immediately; rotate the environment as well to invalidate any cached vault key.
async fn revoke_token(
    State(state): State<AppState>,
    user: AuthUser,
    Path((env_id, token_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    validate_id(&token_id, "token_id")?;
    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    access.require_manage_structure("must be an admin or owner to revoke a machine token")?;
    // Revoke and its audit event commit together, so a failed audit can't leave one un-logged.
    let mut tx = state.pool.begin().await?;
    access
        .require_manage_structure_tx(
            &mut tx,
            "must be an admin or owner to revoke a machine token",
        )
        .await?;
    let revoked = sqlx::query(
        "UPDATE machine_tokens SET revoked_at = now() \
         WHERE id = $1 AND env_id = $2 AND revoked_at IS NULL",
    )
    .bind(&token_id)
    .bind(&env_id)
    .execute(&mut *tx)
    .await?;
    if revoked.rows_affected() == 0 {
        return Err(Error::NotFound("machine token not found".into()));
    }
    if let Some(org) = access.org_id() {
        audit::record_tx(
            &mut tx,
            org,
            &user.user_id,
            "token.revoked",
            audit::Context {
                target: Some(&token_id),
                env_id: Some(&env_id),
                ..Default::default()
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- machine-facing (machine-token-authed, read-only) -------------------------------------------

/// The authenticated machine, resolved from a `Bearer smt_…` token. Scoped to exactly one env.
struct MachineAuth {
    token_id: String,
    env_id: String,
}

impl FromRequestParts<AppState> for MachineAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let token = session::token_from_headers(&parts.headers).ok_or(Error::Unauthorized)?;
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(Error::Unauthorized);
        }
        let row: Option<(String, String)> = sqlx::query_as(concat!(
            "SELECT mt.id, mt.env_id FROM machine_tokens mt \
             JOIN environments e ON e.id = mt.env_id \
             JOIN projects p ON p.id = e.project_id \
             LEFT JOIN organizations o ON o.id = p.org_id \
             WHERE mt.token_hash = $1 AND ",
            active_token_sql!(),
            " AND (o.id IS NULL OR o.lifecycle_state <> 'deleted')",
        ))
        .bind(session::hash_token(&token))
        .fetch_optional(&state.pool)
        .await?;
        let (token_id, env_id) = row.ok_or(Error::Unauthorized)?;
        Ok(MachineAuth { token_id, env_id })
    }
}

#[derive(Serialize)]
struct MachineGrant {
    env_id: String,
    /// The vault key sealed to this machine's public key (base64).
    enc_vault_key: String,
}

/// `GET /machine/grant` - the calling machine's environment id + its own current vault-key grant
/// (re-read per request, so a rotation's re-sealed grant is picked up immediately).
async fn machine_grant(
    State(state): State<AppState>,
    machine: MachineAuth,
) -> Result<Json<MachineGrant>> {
    // Fail closed: if the token row vanished (e.g. an env-deletion cascade in the window after auth),
    // was revoked, or expired in between, answer 401 rather than letting `RowNotFound` bubble up
    // as a 500.
    let enc_vault_key: Option<Vec<u8>> = sqlx::query_scalar(concat!(
        "SELECT enc_vault_key FROM machine_tokens WHERE id = $1 AND ",
        active_token_sql!(),
    ))
    .bind(&machine.token_id)
    .fetch_optional(&state.pool)
    .await?;
    let enc_vault_key = enc_vault_key.ok_or(Error::Unauthorized)?;
    Ok(Json(MachineGrant {
        env_id: machine.env_id,
        enc_vault_key: encoding::encode(&enc_vault_key),
    }))
}

#[derive(Serialize)]
struct MachineSecret {
    id: String,
    enc_name: String,
    enc_value: String,
    enc_data_key: String,
    version: i64,
    deleted: bool,
}

#[derive(Serialize)]
struct MachineSnapshot {
    revision: i64,
    secrets: Vec<MachineSecret>,
}

/// Snapshot row: `(id, enc_name, enc_value, enc_data_key, version, deleted)`.
type SecretRow = (String, Vec<u8>, Vec<u8>, Vec<u8>, i64, bool);

/// `GET /machine/secrets` - the full secret snapshot of the machine's environment (same shape as
/// the user snapshot endpoint, minus the ETag machinery: CI fetches once per run).
async fn machine_secrets(
    State(state): State<AppState>,
    machine: MachineAuth,
) -> Result<Json<MachineSnapshot>> {
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1")
            .bind(&machine.env_id)
            .fetch_optional(&state.pool)
            .await?;
    let revision = revision.ok_or_else(|| Error::NotFound("environment not found".into()))?;

    let rows: Vec<SecretRow> = sqlx::query_as(
        "SELECT id, enc_name, enc_value, enc_data_key, version, (deleted_at IS NOT NULL) \
         FROM secrets WHERE env_id = $1 ORDER BY id",
    )
    .bind(&machine.env_id)
    .fetch_all(&state.pool)
    .await?;

    let secrets = rows
        .into_iter()
        .map(
            |(id, name, value, data_key, version, deleted)| MachineSecret {
                id,
                enc_name: encoding::encode(&name),
                enc_value: encoding::encode(&value),
                enc_data_key: encoding::encode(&data_key),
                version,
                deleted,
            },
        )
        .collect();

    Ok(Json(MachineSnapshot { revision, secrets }))
}

/// Generate a fresh machine token: `smt_` + 256 bits of hex.
fn generate_token() -> String {
    let mut raw = [0u8; TOKEN_BYTES];
    dryoc::rng::copy_randombytes(&mut raw);
    format!("{TOKEN_PREFIX}{}", session::to_hex(&raw))
}
