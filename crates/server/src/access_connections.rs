//! Owner-managed Access connections and staged organisation credentials.

use std::sync::Arc;

use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::access_issuer::AccessIssuer;
use crate::access_proof::{self, DeploymentProofKey};
use crate::auth::{session::hash_token, AuthUser};
use crate::error::{Error, Result};
use crate::org::{self, OrgAccess, Role};
use crate::state::AppState;

const ATTEMPT_SECONDS: i32 = 600;
const PREFIX: &str = "/integrations/access/v1";
const NO_STORE: [(header::HeaderName, &str); 1] = [(header::CACHE_CONTROL, "no-store")];

type Tx<'a> = Transaction<'a, Postgres>;
type JsonResponse<T> = ([(header::HeaderName, &'static str); 1], Json<T>);
type Issuer = Extension<Arc<AccessIssuer>>;

pub(crate) fn router() -> Router<AppState> {
    let routes = Router::new()
        .route("/orgs/{org_id}/connection", get(status).delete(disconnect))
        .route("/orgs/{org_id}/connection/enable", post(enable))
        .route("/orgs/{org_id}/connection/rotate", post(rotate))
        .route("/connection-attempts/redeem", post(redeem))
        .route("/connection-attempts/{attempt_id}/verify", post(verify))
        .route("/connection-attempts/{attempt_id}/confirm", post(confirm));
    #[cfg(feature = "e2e-mock-oauth")]
    let routes = routes.route(
        "/orgs/{org_id}/fixture-active-credential",
        get(fixture_active_credential),
    );
    Router::new().nest(PREFIX, routes)
}

#[derive(sqlx::FromRow)]
struct Attempt {
    id: String,
    org_id: String,
    owner_id: String,
    rotating: bool,
    code_hash: Option<Vec<u8>>,
    staged_hash: Option<Vec<u8>>,
    verified: bool,
    live: bool,
}

#[derive(sqlx::FromRow)]
struct Connection {
    id: String,
    registration: Vec<u8>,
    active_attempt_id: Option<String>,
}

#[derive(Serialize)]
struct Handoff {
    attempt_id: String,
    handoff_url: String,
    expires_in: i32,
}

#[derive(Serialize)]
struct Staged {
    attempt_id: String,
    organisation_id: String,
    credential: String,
    expires_in: i32,
}

#[derive(Serialize)]
struct Receipt {
    attempt_id: String,
    connection_id: String,
}

#[derive(Serialize)]
struct Status {
    connection: &'static str,
    pending: Option<Pending>,
}

#[derive(Serialize)]
struct Pending {
    attempt_id: String,
    kind: &'static str,
    state: &'static str,
}

fn secret(prefix: &str) -> String {
    let mut raw = [0u8; 32];
    dryoc::rng::copy_randombytes(&mut raw);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(raw))
}

fn invalid_attempt() -> Error {
    Error::Conflict("Access connection attempt is unavailable; start a new attempt".into())
}

async fn owner(tx: &mut Tx<'_>, org_id: &str, user: &str) -> Result<OrgAccess> {
    let access = org::access_for_update(tx, org_id, user).await?;
    if access.role() != Role::Owner {
        return Err(Error::Forbidden("organisation owner required".into()));
    }
    Ok(access)
}

async fn connection(tx: &mut Tx<'_>, org_id: &str) -> Result<Option<Connection>> {
    Ok(sqlx::query_as(
        "SELECT id, registration, active_attempt_id FROM access_org_connections WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn begin(
    app: AppState,
    issuer: Arc<AccessIssuer>,
    who: AuthUser,
    org_id: String,
    rotating: bool,
) -> Result<JsonResponse<Handoff>> {
    let mut tx = app.pool.begin().await?;
    owner(&mut tx, &org_id, &who.user_id)
        .await?
        .require_write()?;
    let active = connection(&mut tx, &org_id)
        .await?
        .is_some_and(|row| row.active_attempt_id.is_some());
    if active != rotating {
        return Err(Error::Conflict(if active {
            "Access is already connected; rotate or disconnect it".into()
        } else {
            "Access is disconnected; enable it first".into()
        }));
    }
    sqlx::query(
        "DELETE FROM access_connection_attempts WHERE org_id = $1
                 AND id NOT IN (SELECT active_attempt_id FROM access_org_connections
                                WHERE org_id = $1 AND active_attempt_id IS NOT NULL)",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await?;
    let id = Uuid::new_v4().to_string();
    let code = secret("sacode_");
    sqlx::query("INSERT INTO access_connection_attempts
                 (id, org_id, owner_id, registration, rotating, code_hash, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp() + $7::integer * interval '1 second')")
        .bind(&id).bind(&org_id).bind(&who.user_id).bind(issuer.connection_registration())
        .bind(rotating).bind(hash_token(&code)).bind(ATTEMPT_SECONDS).execute(&mut *tx).await?;
    let mut handoff_url = issuer
        .setup_callback
        .clone()
        .ok_or_else(|| Error::NotConfigured("Access setup callback missing".into()))?;
    // A fragment keeps the handoff code out of HTTP requests and referrers. Access posts it back
    // to its own backend before redeeming it over the signed channel.
    handoff_url.set_fragment(Some(&format!("code={code}")));
    tx.commit().await?;
    Ok((
        NO_STORE,
        Json(Handoff {
            attempt_id: id,
            handoff_url: handoff_url.to_string(),
            expires_in: ATTEMPT_SECONDS,
        }),
    ))
}

async fn enable(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    who: AuthUser,
    Path(org_id): Path<String>,
    headers: HeaderMap,
) -> Result<JsonResponse<Handoff>> {
    require_same_origin(&issuer, &headers)?;
    begin(app, issuer, who, org_id, false).await
}

async fn rotate(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    who: AuthUser,
    Path(org_id): Path<String>,
    headers: HeaderMap,
) -> Result<JsonResponse<Handoff>> {
    require_same_origin(&issuer, &headers)?;
    begin(app, issuer, who, org_id, true).await
}

async fn status(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    who: AuthUser,
    Path(org_id): Path<String>,
) -> Result<JsonResponse<Status>> {
    let mut tx = app.pool.begin().await?;
    owner(&mut tx, &org_id, &who.user_id).await?;
    let current = connection(&mut tx, &org_id).await?;
    let registration = issuer.connection_registration();
    let state = match current.as_ref() {
        Some(row) if row.active_attempt_id.is_some() && row.registration == registration => {
            "active"
        }
        Some(row) if row.active_attempt_id.is_some() => "registration_changed",
        _ => "disconnected",
    };
    let pending: Option<(String, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT id, rotating, staged_hash IS NOT NULL, verified, expires_at > clock_timestamp()
         FROM access_connection_attempts WHERE org_id = $1 AND registration = $2
         AND (code_hash IS NOT NULL OR staged_hash IS NOT NULL)",
    )
    .bind(&org_id)
    .bind(&registration)
    .fetch_optional(&mut *tx)
    .await?;
    let pending = pending.map(|(id, rotating, staged, verified, live)| Pending {
        attempt_id: id,
        kind: if rotating { "rotate" } else { "enable" },
        state: if !live {
            "expired"
        } else if verified {
            "verified"
        } else if staged {
            "staged"
        } else {
            "issued"
        },
    });
    tx.commit().await?;
    Ok((
        NO_STORE,
        Json(Status {
            connection: state,
            pending,
        }),
    ))
}

async fn disconnect(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    who: AuthUser,
    Path(org_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    require_same_origin(&issuer, &headers)?;
    let mut tx = app.pool.begin().await?;
    // Owners can still revoke during an in-progress organisation deletion.
    owner(&mut tx, &org_id, &who.user_id).await?;
    sqlx::query("UPDATE access_org_connections SET active_attempt_id = NULL, credential_hash = NULL,
                 updated_at = clock_timestamp() WHERE org_id = $1 AND active_attempt_id IS NOT NULL")
        .bind(&org_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM access_connection_attempts WHERE org_id = $1")
        .bind(&org_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn require_same_origin(issuer: &AccessIssuer, headers: &HeaderMap) -> Result<()> {
    // A browser can submit some cross-origin requests with a same-site session cookie.
    if headers.get(header::ORIGIN).is_some_and(|origin| {
        origin.to_str().ok() != Some(issuer.origin.as_str().trim_end_matches('/'))
    }) {
        return Err(Error::Forbidden(
            "Access owner changes require the Sotto origin".into(),
        ));
    }
    Ok(())
}

async fn signed_body<T: serde::de::DeserializeOwned>(
    app: &AppState,
    issuer: &AccessIssuer,
    request: Request,
    target: Uri,
) -> Result<T> {
    let key = DeploymentProofKey {
        origin: issuer.origin.as_str().trim_end_matches('/'),
        id: &issuer.client_id,
        key: &issuer.client_key,
    };
    let (proof, body) = access_proof::verify(&key, request, target, true).await?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("DELETE FROM access_deployment_proofs WHERE expires_at <= clock_timestamp()")
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO access_deployment_proofs (proof_hash, expires_at)
                 SELECT $1, to_timestamp($2) WHERE to_timestamp($2) > clock_timestamp()
                 ON CONFLICT DO NOTHING",
    )
    .bind(hash_token(&format!(
        "{}\n{}",
        issuer.client_id, proof.nonce
    )))
    .bind(proof.expires as f64)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(Error::Unauthorized);
    }
    tx.commit().await?;
    serde_json::from_slice(&body)
        .map_err(|_| Error::BadRequest("invalid Access request body".into()))
}

async fn locked_attempt(tx: &mut Tx<'_>, registration: &[u8], attempt_id: &str) -> Result<Attempt> {
    let org_id: String = sqlx::query_scalar(
        "SELECT org_id FROM access_connection_attempts WHERE id = $1 AND registration = $2",
    )
    .bind(attempt_id)
    .bind(registration)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(invalid_attempt)?;
    sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(org_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(invalid_attempt)?;
    // A disconnect or restart of setup may have removed this row while we waited for the org lock.
    sqlx::query_as(
        "SELECT id, org_id, owner_id, rotating, code_hash, staged_hash, verified,
                    expires_at > clock_timestamp() AS live
                    FROM access_connection_attempts WHERE id = $1 AND registration = $2",
    )
    .bind(attempt_id)
    .bind(registration)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(invalid_attempt)
}

async fn require_attempt_owner(tx: &mut Tx<'_>, attempt: &Attempt) -> Result<()> {
    owner(tx, &attempt.org_id, &attempt.owner_id)
        .await?
        .require_write()
}

#[derive(Deserialize)]
struct Redeem {
    code: String,
}

async fn redeem(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    OriginalUri(target): OriginalUri,
    request: Request,
) -> Result<JsonResponse<Staged>> {
    let input: Redeem = signed_body(&app, &issuer, request, target).await?;
    let registration = issuer.connection_registration();
    let code_hash = hash_token(&input.code);
    let mut tx = app.pool.begin().await?;
    let id: String = sqlx::query_scalar(
        "SELECT id FROM access_connection_attempts WHERE code_hash = $1 AND registration = $2",
    )
    .bind(&code_hash)
    .bind(&registration)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(invalid_attempt)?;
    let attempt = locked_attempt(&mut tx, &registration, &id).await?;
    if !attempt.live || attempt.code_hash.as_ref() != Some(&code_hash) {
        return Err(invalid_attempt());
    }
    require_attempt_owner(&mut tx, &attempt).await?;
    let credential = secret("sac_");
    sqlx::query(
        "UPDATE access_connection_attempts SET code_hash = NULL, staged_hash = $2,
                 expires_at = clock_timestamp() + $3::integer * interval '1 second' WHERE id = $1",
    )
    .bind(&attempt.id)
    .bind(hash_token(&credential))
    .bind(ATTEMPT_SECONDS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        NO_STORE,
        Json(Staged {
            attempt_id: attempt.id,
            organisation_id: attempt.org_id,
            credential,
            expires_in: ATTEMPT_SECONDS,
        }),
    ))
}

fn bearer(headers: &HeaderMap) -> Result<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.starts_with("sac_"))
        .ok_or(Error::Unauthorized)
}

async fn verify(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let hash = hash_token(bearer(&headers)?);
    let mut tx = app.pool.begin().await?;
    let attempt = locked_attempt(&mut tx, &issuer.connection_registration(), &id).await?;
    if !attempt.live || attempt.staged_hash.as_ref() != Some(&hash) {
        return Err(Error::Unauthorized);
    }
    require_attempt_owner(&mut tx, &attempt).await?;
    sqlx::query("UPDATE access_connection_attempts SET verified = TRUE WHERE id = $1")
        .bind(&attempt.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Validate an organisation's active metadata credential on the caller's database connection.
/// The caller supplies the organisation from the resource being read, never from the credential.
pub async fn active_credential(
    connection: &mut PgConnection,
    issuer: &AccessIssuer,
    org_id: &str,
    credential: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM access_org_connections c JOIN organizations o ON o.id = c.org_id
         WHERE c.org_id = $1 AND c.registration = $2 AND c.credential_hash = $3
         AND c.active_attempt_id IS NOT NULL AND o.lifecycle_state = 'active')")
        .bind(org_id).bind(issuer.connection_registration()).bind(hash_token(credential))
        .fetch_one(connection).await?)
}

#[cfg(feature = "e2e-mock-oauth")]
async fn fixture_active_credential(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    Path(org_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let credential = bearer(&headers)?;
    let mut connection = app.pool.acquire().await?;
    if !active_credential(&mut connection, &issuer, &org_id, credential).await? {
        return Err(Error::Unauthorized);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Confirm {
    credential: String,
}

async fn confirm(
    State(app): State<AppState>,
    Extension(issuer): Issuer,
    Path(id): Path<String>,
    OriginalUri(target): OriginalUri,
    request: Request,
) -> Result<JsonResponse<Receipt>> {
    let input: Confirm = signed_body(&app, &issuer, request, target).await?;
    let registration = issuer.connection_registration();
    let mut tx = app.pool.begin().await?;
    let attempt = locked_attempt(&mut tx, &registration, &id).await?;
    let current = connection(&mut tx, &attempt.org_id).await?;
    if let Some(row) = current
        .as_ref()
        .filter(|row| row.active_attempt_id.as_deref() == Some(&id))
    {
        if !active_credential(&mut tx, &issuer, &attempt.org_id, &input.credential).await? {
            return Err(Error::Unauthorized);
        }
        tx.commit().await?;
        return Ok((
            NO_STORE,
            Json(Receipt {
                attempt_id: id,
                connection_id: row.id.clone(),
            }),
        ));
    }
    if !attempt.live || !attempt.verified {
        return Err(invalid_attempt());
    }
    let hash = hash_token(&input.credential);
    if attempt.staged_hash.as_ref() != Some(&hash) {
        return Err(Error::Unauthorized);
    }
    require_attempt_owner(&mut tx, &attempt).await?;
    let active = current.filter(|row| row.active_attempt_id.is_some());
    let connection_id = match (attempt.rotating, active) {
        (true, Some(row)) => row.id,
        (false, None) => Uuid::new_v4().to_string(),
        _ => return Err(invalid_attempt()),
    };
    sqlx::query("INSERT INTO access_org_connections (org_id, id, registration, active_attempt_id, credential_hash)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (org_id) DO UPDATE SET id = EXCLUDED.id, registration = EXCLUDED.registration,
                 active_attempt_id = EXCLUDED.active_attempt_id, credential_hash = EXCLUDED.credential_hash,
                 updated_at = clock_timestamp()")
        .bind(&attempt.org_id).bind(&connection_id).bind(&registration).bind(&id).bind(hash)
        .execute(&mut *tx).await?;
    sqlx::query("UPDATE access_connection_attempts SET staged_hash = NULL WHERE id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM access_connection_attempts WHERE org_id = $1 AND id <> $2")
        .bind(&attempt.org_id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO access_prior_links (org_id, registration) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(&attempt.org_id).bind(&registration).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        NO_STORE,
        Json(Receipt {
            attempt_id: id,
            connection_id,
        }),
    ))
}
