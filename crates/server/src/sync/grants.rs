//! Per-member environment vault-key grants — the crypto capability to *decrypt* a shared
//! environment (PR3a's access only gates seeing the ciphertext).
//!
//! A grant is the env vault key sealed to a member's public key: server-opaque bytes the recipient
//! opens with their private key. Sharing (`POST .../grants`) is an admin+/owner action, restricted
//! to org environments and to members of that org. A member fetches their own grant with
//! `GET .../grant`; not having one is a `404` even when they can otherwise reach the environment.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::org;
use crate::state::AppState;
use crate::sync::access::env_access;
use crate::sync::MAX_ENC_KEY;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/environments/{env_id}/grants", post(create_grant))
        .route("/environments/{env_id}/grant", get(get_grant))
}

#[derive(Deserialize)]
struct CreateGrant {
    /// The org member the vault key is sealed to.
    user_id: String,
    /// The vault key sealed to that member's public key.
    enc_vault_key: String,
}

#[derive(Serialize)]
struct GrantView {
    enc_vault_key: String,
}

/// `POST /environments/{env_id}/grants` — share an environment by storing a grant for a member.
/// Admin+/owner only; the environment must belong to an organization and the target must be a
/// member of it. Idempotent: re-granting (e.g. after a key rotation) overwrites the stored grant.
async fn create_grant(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    Json(body): Json<CreateGrant>,
) -> Result<StatusCode> {
    let enc_vault_key = encoding::decode(&body.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?;

    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    if !access.can_manage_structure() {
        return Err(Error::Forbidden(
            "must be an admin or owner to share an environment".into(),
        ));
    }

    // Sharing is an org operation: the env must be org-owned, and the recipient a member of that org
    // (a grant to someone who can't reach the env would be dead weight).
    let org_id: Option<String> = sqlx::query_scalar(
        "SELECT p.org_id FROM environments e JOIN projects p ON e.project_id = p.id WHERE e.id = $1",
    )
    .bind(&env_id)
    .fetch_one(&state.pool)
    .await?;
    let org_id = org_id.ok_or_else(|| {
        Error::BadRequest("environment is not in an organization; nothing to share".into())
    })?;
    if org::role_of(&state.pool, &org_id, &body.user_id)
        .await?
        .is_none()
    {
        return Err(Error::BadRequest(
            "target user is not a member of this organization".into(),
        ));
    }

    sqlx::query(
        "INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (env_id, user_id) \
         DO UPDATE SET enc_vault_key = EXCLUDED.enc_vault_key, granted_by = EXCLUDED.granted_by",
    )
    .bind(&env_id)
    .bind(&body.user_id)
    .bind(&enc_vault_key)
    .bind(&user.user_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::OK)
}

/// `GET /environments/{env_id}/grant` — the caller's own vault-key grant for the environment. `404`
/// if the caller has no grant, even when org access otherwise lets them see the ciphertext.
async fn get_grant(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
) -> Result<Json<GrantView>> {
    env_access(&state, &env_id, &user.user_id).await?;

    let enc_vault_key: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT enc_vault_key FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(&env_id)
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;
    let enc_vault_key =
        enc_vault_key.ok_or_else(|| Error::NotFound("no grant for this environment".into()))?;

    Ok(Json(GrantView {
        enc_vault_key: encoding::encode(&enc_vault_key),
    }))
}
