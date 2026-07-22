//! Per-member environment vault-key grants - the crypto capability to *decrypt* a shared
//! environment (PR3a's access only gates seeing the ciphertext).
//!
//! A grant is the env vault key sealed to a member's public key: server-opaque bytes the recipient
//! opens with their private key. Sharing (`POST .../grants`) is an admin+/owner action, restricted
//! to org environments and to members of that org. A member fetches their own grant with
//! `GET .../grant`; not having one is a `404` even when they can otherwise reach the environment.
//!
//! `POST .../rotate` re-keys an environment: the client generates a new vault key, rewraps every
//! secret's data key under it, and re-grants the remaining members. The server applies the new
//! grants (replacing the old set, so a removed member's grant is dropped) and the rewrapped data
//! keys in one transaction, guarded by optimistic concurrency on the environment revision.

use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::audit;
use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::org;
use crate::state::AppState;
use crate::sync::access::env_access;
use crate::sync::MAX_ENC_KEY;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/environments/{env_id}/grants",
            post(create_grant).get(list_grant_holders),
        )
        .route("/environments/{env_id}/grant", get(get_grant))
        .route("/environments/{env_id}/rotate", post(rotate))
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

#[derive(Serialize)]
struct GrantHolder {
    user_id: String,
}

/// `GET /environments/{env_id}/grants` - the user ids currently granted this environment (admin+),
/// so a rotation knows who to re-grant.
async fn list_grant_holders(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
) -> Result<Json<Vec<GrantHolder>>> {
    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    if !access.can_manage_structure() {
        return Err(Error::Forbidden(
            "must be an admin or owner to list grant holders".into(),
        ));
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM environment_grants WHERE env_id = $1 ORDER BY user_id",
    )
    .bind(&env_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|user_id| GrantHolder { user_id })
            .collect(),
    ))
}

/// `POST /environments/{env_id}/grants` - share an environment by storing a grant for a member.
/// Admin+/owner only; the environment must belong to an organisation and the target must be a
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
        Error::BadRequest("environment is not in an organisation; nothing to share".into())
    })?;
    if org::role_of(&state.pool, &org_id, &body.user_id)
        .await?
        .is_none()
    {
        return Err(Error::BadRequest(
            "target user is not a member of this organisation".into(),
        ));
    }

    // Grant and its audit event commit together, so a failed audit can't leave an un-logged share.
    let mut tx = state.pool.begin().await?;
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
    .execute(&mut *tx)
    .await?;
    audit::record_tx(
        &mut tx,
        &org_id,
        &user.user_id,
        "env.shared",
        audit::Context {
            target: Some(&body.user_id),
            env_id: Some(&env_id),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// `GET /environments/{env_id}/grant` - the caller's own vault-key grant for the environment. `404`
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

#[derive(Deserialize)]
struct RotateRequest {
    /// The env revision the rewrap was computed against; the rotation applies only if it still holds.
    base_revision: i64,
    /// The new grant set (the new vault key sealed to each remaining member). It *replaces* the whole
    /// grant set, so any member not listed here (e.g. one just removed) loses access.
    grants: Vec<GrantEntry>,
    /// Every current secret's data key, rewrapped under the new vault key.
    data_keys: Vec<DataKeyEntry>,
    /// The new vault key re-sealed to every *active* machine token's public key. Must cover exactly
    /// the env's active tokens (revoke a token first to drop it), so rotation never strands CI.
    #[serde(default)]
    machine_grants: Vec<MachineGrantEntry>,
    /// Every retained history version's data key, rewrapped under the new vault key. Must cover
    /// exactly the env's `secret_versions` rows, so history never silently dies at a rotation.
    #[serde(default)]
    history_keys: Vec<HistoryKeyEntry>,
}

#[derive(Deserialize)]
struct HistoryKeyEntry {
    secret_id: String,
    version: i64,
    enc_data_key: String,
}

#[derive(Deserialize)]
struct GrantEntry {
    user_id: String,
    enc_vault_key: String,
}

#[derive(Deserialize)]
struct MachineGrantEntry {
    token_id: String,
    enc_vault_key: String,
}

#[derive(Deserialize)]
struct DataKeyEntry {
    secret_id: String,
    enc_data_key: String,
}

#[derive(Serialize)]
struct RotateResponse {
    revision: i64,
}

/// `POST /environments/{env_id}/rotate` - re-key an environment. Admin+/owner only, org-owned envs
/// only. In one transaction: verify `base_revision`, rewrap every secret's data key, replace the
/// grant set (dropping anyone not re-granted), repoint the inline vault key at the caller's new
/// grant, and bump the revision. Fails closed with 412 if a concurrent write moved the revision, or
/// 400 if the data keys don't cover exactly the env's secrets, the caller omitted their own grant,
/// or the grant set names a non-member, a duplicate user, or a duplicate machine token.
async fn rotate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(env_id): Path<String>,
    Json(req): Json<RotateRequest>,
) -> Result<Json<RotateResponse>> {
    let (_project_id, access) = env_access(&state, &env_id, &user.user_id).await?;
    if !access.can_manage_structure() {
        return Err(Error::Forbidden(
            "must be an admin or owner to rotate an environment".into(),
        ));
    }

    // Rotation re-grants org members, so the env must be org-owned.
    let org_id: Option<String> = sqlx::query_scalar(
        "SELECT p.org_id FROM environments e JOIN projects p ON e.project_id = p.id WHERE e.id = $1",
    )
    .bind(&env_id)
    .fetch_one(&state.pool)
    .await?;
    let org_id =
        org_id.ok_or_else(|| Error::BadRequest("environment is not in an organisation".into()))?;

    // Every grantee must be a distinct member of that org - the same rule `create_grant` enforces.
    // Without the membership check an admin could re-grant the env to a non-member; without the
    // duplicate check a repeated user_id would trip the environment_grants PK mid-transaction and
    // surface as a 500 instead of a clean 400.
    let member_rows: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM organization_memberships WHERE org_id = $1")
            .bind(&org_id)
            .fetch_all(&state.pool)
            .await?;
    let members: HashSet<&str> = member_rows.iter().map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::with_capacity(req.grants.len());
    for g in &req.grants {
        if !seen.insert(g.user_id.as_str()) {
            return Err(Error::BadRequest(
                "duplicate user_id in the rotation grant set".into(),
            ));
        }
        if !members.contains(g.user_id.as_str()) {
            return Err(Error::BadRequest(
                "a grant recipient is not a member of this organisation".into(),
            ));
        }
    }

    // Decode all blobs up front (bounds-checked), so a bad field fails before we touch anything.
    let mut grants = Vec::with_capacity(req.grants.len());
    for g in &req.grants {
        grants.push((
            g.user_id.clone(),
            encoding::decode(&g.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?,
        ));
    }
    let mut data_keys = Vec::with_capacity(req.data_keys.len());
    for d in &req.data_keys {
        data_keys.push((
            d.secret_id.clone(),
            encoding::decode(&d.enc_data_key, "enc_data_key", MAX_ENC_KEY)?,
        ));
    }
    // Reject a duplicate token_id up front, as with user grants: the coverage check below dedups
    // via a set, so a repeat would slip through and drive an ambiguous double-UPDATE of one token.
    let mut machine_grants = Vec::with_capacity(req.machine_grants.len());
    let mut seen_tokens: HashSet<&str> = HashSet::with_capacity(req.machine_grants.len());
    for m in &req.machine_grants {
        if !seen_tokens.insert(m.token_id.as_str()) {
            return Err(Error::BadRequest(
                "duplicate token_id in the rotation machine-grant set".into(),
            ));
        }
        machine_grants.push((
            m.token_id.clone(),
            encoding::decode(&m.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?,
        ));
    }
    // Rewrapped history keys, keyed by (secret_id, version); duplicates rejected like the others.
    let mut history_keys = Vec::with_capacity(req.history_keys.len());
    let mut seen_history: HashSet<(&str, i64)> = HashSet::with_capacity(req.history_keys.len());
    for h in &req.history_keys {
        if !seen_history.insert((h.secret_id.as_str(), h.version)) {
            return Err(Error::BadRequest(
                "duplicate (secret_id, version) in the rotation history set".into(),
            ));
        }
        history_keys.push((
            h.secret_id.clone(),
            h.version,
            encoding::decode(&h.enc_data_key, "enc_data_key", MAX_ENC_KEY)?,
        ));
    }

    // The caller must keep their own grant, or they'd lock themselves out of the env they just rekeyed.
    if !grants.iter().any(|(uid, _)| *uid == user.user_id) {
        return Err(Error::BadRequest(
            "you must include your own grant in the rotation".into(),
        ));
    }

    let mut tx = state.pool.begin().await?;

    // Lock the environment row so the rotation serialises against concurrent secret writes.
    let current: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1 FOR UPDATE")
            .bind(&env_id)
            .fetch_optional(&mut *tx)
            .await?;
    let current = current.ok_or_else(|| Error::NotFound("environment not found".into()))?;
    if current != req.base_revision {
        return Err(Error::Precondition(
            "base_revision is stale; re-pull and re-rotate".into(),
        ));
    }

    // The rewrapped data keys must cover exactly the env's secrets - none left under the old key, and
    // none for a secret that isn't here (a mismatch means the client rewrapped a stale snapshot).
    verify_covers_all_secrets(&mut tx, &env_id, &data_keys).await?;

    // History must be covered exactly too - a version left under the old key would silently become
    // unreadable, and a row that isn't there means the client rewrapped a stale view.
    let existing_history: Vec<(String, i64)> = sqlx::query_as(
        "SELECT sv.secret_id, sv.version FROM secret_versions sv \
         JOIN secrets s ON sv.secret_id = s.id WHERE s.env_id = $1",
    )
    .bind(&env_id)
    .fetch_all(&mut *tx)
    .await?;
    let existing_set: HashSet<(&str, i64)> = existing_history
        .iter()
        .map(|(id, v)| (id.as_str(), *v))
        .collect();
    let provided_set: HashSet<(&str, i64)> = history_keys
        .iter()
        .map(|(id, v, _)| (id.as_str(), *v))
        .collect();
    if provided_set != existing_set {
        return Err(Error::BadRequest(
            "rotation must rewrap exactly the environment's retained history versions".into(),
        ));
    }
    for (secret_id, version, enc_data_key) in &history_keys {
        sqlx::query(
            "UPDATE secret_versions SET enc_data_key = $3 WHERE secret_id = $1 AND version = $2",
        )
        .bind(secret_id)
        .bind(version)
        .bind(enc_data_key)
        .execute(&mut *tx)
        .await?;
    }

    // Machine grants must cover exactly the env's *active* tokens: leaving one out would strand its
    // CI on the old key with no way to notice (revoke a token first to genuinely drop it).
    let active_tokens: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM machine_tokens WHERE env_id = $1 AND revoked_at IS NULL",
    )
    .bind(&env_id)
    .fetch_all(&mut *tx)
    .await?;
    let active: HashSet<&str> = active_tokens.iter().map(String::as_str).collect();
    let provided: HashSet<&str> = machine_grants.iter().map(|(id, _)| id.as_str()).collect();
    if provided != active {
        return Err(Error::BadRequest(
            "rotation must re-grant exactly the environment's active machine tokens".into(),
        ));
    }
    for (token_id, enc_vault_key) in &machine_grants {
        sqlx::query("UPDATE machine_tokens SET enc_vault_key = $3 WHERE id = $1 AND env_id = $2")
            .bind(token_id)
            .bind(&env_id)
            .bind(enc_vault_key)
            .execute(&mut *tx)
            .await?;
    }
    for (secret_id, enc_data_key) in &data_keys {
        sqlx::query("UPDATE secrets SET enc_data_key = $3 WHERE id = $1 AND env_id = $2")
            .bind(secret_id)
            .bind(&env_id)
            .bind(enc_data_key)
            .execute(&mut *tx)
            .await?;
    }

    // Replace the grant set wholesale: this is what drops a removed member's grant.
    sqlx::query("DELETE FROM environment_grants WHERE env_id = $1")
        .bind(&env_id)
        .execute(&mut *tx)
        .await?;
    for (user_id, enc_vault_key) in &grants {
        sqlx::query(
            "INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&env_id)
        .bind(user_id)
        .bind(enc_vault_key)
        .bind(&user.user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_fk_violation(&e) {
                Error::BadRequest("a grant recipient is not a known user".into())
            } else {
                e.into()
            }
        })?;
    }

    let new_revision = current + 1;
    sqlx::query("UPDATE environments SET revision = $2 WHERE id = $1")
        .bind(&env_id)
        .bind(new_revision)
        .execute(&mut *tx)
        .await?;
    let detail = format!(
        "{} member grant(s), {} machine grant(s), {} history version(s)",
        grants.len(),
        machine_grants.len(),
        history_keys.len()
    );
    audit::record_tx(
        &mut tx,
        &org_id,
        &user.user_id,
        "env.rotated",
        audit::Context {
            env_id: Some(&env_id),
            detail: Some(&detail),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;

    Ok(Json(RotateResponse {
        revision: new_revision,
    }))
}

/// Verify the provided data keys reference exactly the environment's current secrets (live +
/// tombstoned), so no secret is stranded under the old vault key.
async fn verify_covers_all_secrets(
    tx: &mut Transaction<'_, Postgres>,
    env_id: &str,
    data_keys: &[(String, Vec<u8>)],
) -> Result<()> {
    let existing: Vec<String> = sqlx::query_scalar("SELECT id FROM secrets WHERE env_id = $1")
        .bind(env_id)
        .fetch_all(&mut **tx)
        .await?;
    let existing: HashSet<&str> = existing.iter().map(String::as_str).collect();
    let provided: HashSet<&str> = data_keys.iter().map(|(id, _)| id.as_str()).collect();
    if provided != existing {
        return Err(Error::BadRequest(
            "rotation must rewrap exactly the environment's current secrets".into(),
        ));
    }
    Ok(())
}

fn is_fk_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}
