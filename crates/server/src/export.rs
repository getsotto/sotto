//! Bounded, resumable account export.
//!
//! The server emits only its existing opaque ciphertext and wrapped-key material. A short-lived
//! manifest fixes the authorised resource set and each environment revision before the first
//! chunk is read; every later chunk rechecks the grant and revision, so a revocation or concurrent
//! write makes the export restart instead of claiming a complete but mixed snapshot.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::sync::access::env_access;

const EXPORT_VERSION: i32 = 1;
const EXPORT_TTL_MINUTES: i64 = 15;
const MAX_PROJECTS: usize = 256;
const MAX_ENVIRONMENTS: usize = 1024;
const MAX_SECRETS_PER_ENVIRONMENT: usize = 4096;
const MAX_HISTORY_PER_ENVIRONMENT: usize = 16_384;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/account/export", post(start_export))
        .route(
            "/account/export/{export_id}/chunks/{index}",
            get(export_chunk),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportAccount {
    public_key: String,
    enc_private_keys: String,
    kdf_params: String,
    recovery_blob: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportProject {
    id: String,
    enc_name: String,
    org_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportEnvironmentRef {
    id: String,
    project_id: String,
    enc_name: String,
    enc_vault_key: String,
    revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportManifest {
    version: i32,
    account: ExportAccount,
    projects: Vec<ExportProject>,
    environments: Vec<ExportEnvironmentRef>,
    /// Organisation environments the caller can see but has never been granted.
    #[serde(default)]
    not_shared_environment_ids: Vec<String>,
    omitted_environment_count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ExportManifestView {
    version: i32,
    export_id: String,
    manifest_hash: String,
    expires_at: String,
    total_chunks: usize,
    complete: bool,
    projects: Vec<ExportProject>,
    environments: Vec<ExportEnvironmentRef>,
    /// Organisation environments intentionally outside this caller's exportable scope.
    not_shared_environment_ids: Vec<String>,
    omitted_environment_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportSecret {
    id: String,
    enc_name: String,
    enc_value: String,
    enc_data_key: String,
    version: i64,
    deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExportHistory {
    secret_id: String,
    version: i64,
    enc_name: String,
    enc_value: String,
    enc_data_key: String,
}

#[derive(Debug, Clone, Serialize)]
struct ExportEnvironment {
    id: String,
    project_id: String,
    enc_name: String,
    enc_vault_key: String,
    revision: i64,
    content_hash: String,
    secrets: Vec<ExportSecret>,
    history: Vec<ExportHistory>,
}

fn environment_content_hash(environment: &ExportEnvironment) -> Result<String> {
    let mut payload = environment.clone();
    payload.content_hash.clear();
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::Internal(format!("serialising export chunk: {e}")))?;
    Ok(encoding::encode(&Sha256::digest(bytes)))
}

#[derive(Debug, Clone, Serialize)]
struct ExportChunk {
    version: i32,
    export_id: String,
    manifest_hash: String,
    index: usize,
    total_chunks: usize,
    complete: bool,
    account: Option<ExportAccount>,
    environment: Option<ExportEnvironment>,
}

async fn start_export(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<(StatusCode, Json<ExportManifestView>)> {
    sqlx::query("DELETE FROM export_sessions WHERE expires_at <= now()")
        .execute(&state.pool)
        .await?;
    type AccountRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);
    let account: Option<AccountRow> = sqlx::query_as(
        "SELECT public_key, enc_private_keys, kdf_params, recovery_blob \
         FROM users WHERE id = $1 AND public_key IS NOT NULL",
    )
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;
    let (public_key, enc_private_keys, kdf_params, recovery_blob) =
        account.ok_or_else(|| Error::NotFound("account is not initialised".into()))?;

    let project_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM projects p \
         WHERE (p.org_id IS NULL AND p.owner_id = $1) \
            OR (p.org_id IS NOT NULL AND EXISTS ( \
                   SELECT 1 FROM organization_memberships m \
                   JOIN organizations o ON o.id = m.org_id \
                   WHERE m.org_id = p.org_id AND m.user_id = $1 \
                     AND o.lifecycle_state <> 'deleted'))",
    )
    .bind(&user.user_id)
    .fetch_one(&state.pool)
    .await?;
    if project_count > MAX_PROJECTS as i64 {
        return Err(Error::Conflict(
            "export exceeds the project bound; narrow the scope".into(),
        ));
    }
    let environment_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environments e JOIN projects p ON p.id = e.project_id \
         WHERE (p.org_id IS NULL AND p.owner_id = $1) \
            OR (p.org_id IS NOT NULL AND EXISTS ( \
                   SELECT 1 FROM organization_memberships m \
                   JOIN organizations o ON o.id = m.org_id \
                   WHERE m.org_id = p.org_id AND m.user_id = $1 \
                     AND o.lifecycle_state <> 'deleted'))",
    )
    .bind(&user.user_id)
    .fetch_one(&state.pool)
    .await?;
    if environment_count > MAX_ENVIRONMENTS as i64 {
        return Err(Error::Conflict(
            "export exceeds the environment bound; narrow the scope".into(),
        ));
    }

    type ResourceRow = (
        String,
        Vec<u8>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<i64>,
    );
    let rows: Vec<ResourceRow> = sqlx::query_as(
        "SELECT p.id, p.enc_name, p.org_id, e.id, e.enc_name, eg.enc_vault_key, e.revision \
         FROM projects p \
         LEFT JOIN environments e ON e.project_id = p.id \
         LEFT JOIN environment_grants eg ON eg.env_id = e.id AND eg.user_id = $1 \
         WHERE (p.org_id IS NULL AND p.owner_id = $1) \
            OR (p.org_id IS NOT NULL AND EXISTS ( \
                   SELECT 1 FROM organization_memberships m \
                   JOIN organizations o ON o.id = m.org_id \
                   WHERE m.org_id = p.org_id AND m.user_id = $1 \
                     AND o.lifecycle_state <> 'deleted')) \
         ORDER BY p.id, e.id LIMIT $2",
    )
    .bind(&user.user_id)
    .bind((MAX_PROJECTS + MAX_ENVIRONMENTS + 1) as i64)
    .fetch_all(&state.pool)
    .await?;
    if rows.len() > MAX_PROJECTS + MAX_ENVIRONMENTS {
        return Err(Error::Conflict(
            "export exceeds the project or environment bound; narrow the scope".into(),
        ));
    }

    let mut projects = Vec::new();
    let mut environments = Vec::new();
    let mut not_shared_environment_ids = Vec::new();
    for (project_id, enc_name, org_id, env_id, env_name, grant, revision) in rows {
        if projects.last().map(|p: &ExportProject| p.id.as_str()) != Some(project_id.as_str()) {
            projects.push(ExportProject {
                id: project_id.clone(),
                enc_name: encoding::encode(&enc_name),
                org_id,
            });
        }
        let Some(env_id) = env_id else { continue };
        let Some(grant) = grant else {
            not_shared_environment_ids.push(env_id);
            continue;
        };
        environments.push(ExportEnvironmentRef {
            id: env_id,
            project_id,
            enc_name: encoding::encode(&env_name.expect("environment name")),
            enc_vault_key: encoding::encode(&grant),
            revision: revision.expect("environment revision"),
        });
    }

    let document = ExportManifest {
        version: EXPORT_VERSION,
        account: ExportAccount {
            public_key: encoding::encode(&public_key),
            enc_private_keys: encoding::encode(&enc_private_keys),
            kdf_params: encoding::encode(&kdf_params),
            recovery_blob: encoding::encode(&recovery_blob),
        },
        projects: projects.clone(),
        environments: environments.clone(),
        omitted_environment_count: not_shared_environment_ids.len(),
        not_shared_environment_ids: not_shared_environment_ids.clone(),
    };
    let manifest = serde_json::to_vec(&document)
        .map_err(|e| Error::Internal(format!("serialising export manifest: {e}")))?;
    let hash = Sha256::digest(&manifest).to_vec();
    let hash_text = encoding::encode(&hash);
    let export_id = uuid::Uuid::new_v4().simple().to_string();
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO export_sessions (id, user_id, manifest, manifest_hash, expires_at) \
         VALUES ($1, $2, $3, $4, now() + ($5::bigint * interval '1 minute')) \
         RETURNING expires_at::text",
    )
    .bind(&export_id)
    .bind(&user.user_id)
    .bind(&manifest)
    .bind(&hash)
    .bind(EXPORT_TTL_MINUTES)
    .fetch_one(&state.pool)
    .await?;

    let total_chunks = 1 + environments.len();
    Ok((
        StatusCode::CREATED,
        Json(ExportManifestView {
            version: EXPORT_VERSION,
            export_id,
            manifest_hash: hash_text,
            expires_at,
            total_chunks,
            complete: true,
            projects,
            environments,
            not_shared_environment_ids,
            omitted_environment_count: document.omitted_environment_count,
        }),
    ))
}

async fn export_chunk(
    State(state): State<AppState>,
    user: AuthUser,
    Path((export_id, index)): Path<(String, usize)>,
) -> Result<Json<ExportChunk>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT manifest, manifest_hash FROM export_sessions \
         WHERE id = $1 AND user_id = $2 AND expires_at > now()",
    )
    .bind(&export_id)
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;
    let (manifest, hash) =
        row.ok_or_else(|| Error::NotFound("export is missing or expired".into()))?;
    let document: ExportManifest = serde_json::from_slice(&manifest)
        .map_err(|_| Error::Internal("stored export manifest is corrupt".into()))?;
    if index > document.environments.len() {
        return Err(Error::BadRequest("export chunk is out of range".into()));
    }
    let hash_text = encoding::encode(&hash);
    if index == 0 {
        return Ok(Json(ExportChunk {
            version: document.version,
            export_id,
            manifest_hash: hash_text,
            index,
            total_chunks: 1 + document.environments.len(),
            complete: document.environments.is_empty(),
            account: Some(document.account),
            environment: None,
        }));
    }

    let expected = &document.environments[index - 1];
    env_access(&state, &expected.id, &user.user_id)
        .await
        .map_err(|_| Error::Conflict("export scope changed; restart the export".into()))?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let current: Option<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT e.revision, eg.enc_vault_key FROM environments e \
         JOIN environment_grants eg ON eg.env_id = e.id AND eg.user_id = $2 \
         WHERE e.id = $1 FOR UPDATE",
    )
    .bind(&expected.id)
    .bind(&user.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (revision, grant) = current
        .ok_or_else(|| Error::Conflict("export scope changed; restart the export".into()))?;
    if revision != expected.revision || encoding::encode(&grant) != expected.enc_vault_key {
        return Err(Error::Conflict(
            "export changed while it was being read; restart the export".into(),
        ));
    }
    let secret_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE env_id = $1")
        .bind(&expected.id)
        .fetch_one(&mut *tx)
        .await?;
    if secret_count > MAX_SECRETS_PER_ENVIRONMENT as i64 {
        return Err(Error::Conflict(
            "export environment exceeds the secret bound; export in smaller scopes".into(),
        ));
    }
    type SecretRow = (String, Vec<u8>, Vec<u8>, Vec<u8>, i64, bool);
    let secrets: Vec<SecretRow> = sqlx::query_as(
        "SELECT id, enc_name, enc_value, enc_data_key, version, (deleted_at IS NOT NULL) \
         FROM secrets WHERE env_id = $1 ORDER BY id",
    )
    .bind(&expected.id)
    .fetch_all(&mut *tx)
    .await?;
    let history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM secret_versions sv \
         JOIN secrets s ON sv.secret_id = s.id WHERE s.env_id = $1",
    )
    .bind(&expected.id)
    .fetch_one(&mut *tx)
    .await?;
    if history_count > MAX_HISTORY_PER_ENVIRONMENT as i64 {
        return Err(Error::Conflict(
            "export history exceeds the row bound; export in smaller scopes".into(),
        ));
    }
    type HistoryRow = (String, i64, Vec<u8>, Vec<u8>, Vec<u8>);
    let history: Vec<HistoryRow> = sqlx::query_as(
        "SELECT sv.secret_id, sv.version, sv.enc_name, sv.enc_value, sv.enc_data_key \
         FROM secret_versions sv JOIN secrets s ON sv.secret_id = s.id \
         WHERE s.env_id = $1 ORDER BY sv.secret_id, sv.version",
    )
    .bind(&expected.id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut environment = ExportEnvironment {
        id: expected.id.clone(),
        project_id: expected.project_id.clone(),
        enc_name: expected.enc_name.clone(),
        enc_vault_key: encoding::encode(&grant),
        revision,
        content_hash: String::new(),
        secrets: secrets
            .into_iter()
            .map(
                |(id, enc_name, enc_value, enc_data_key, version, deleted)| ExportSecret {
                    id,
                    enc_name: encoding::encode(&enc_name),
                    enc_value: encoding::encode(&enc_value),
                    enc_data_key: encoding::encode(&enc_data_key),
                    version,
                    deleted,
                },
            )
            .collect(),
        history: history
            .into_iter()
            .map(
                |(secret_id, version, enc_name, enc_value, enc_data_key)| ExportHistory {
                    secret_id,
                    version,
                    enc_name: encoding::encode(&enc_name),
                    enc_value: encoding::encode(&enc_value),
                    enc_data_key: encoding::encode(&enc_data_key),
                },
            )
            .collect(),
    };
    environment.content_hash = environment_content_hash(&environment)?;
    Ok(Json(ExportChunk {
        version: document.version,
        export_id,
        manifest_hash: hash_text,
        index,
        total_chunks: 1 + document.environments.len(),
        complete: index + 1 == 1 + document.environments.len(),
        account: None,
        environment: Some(environment),
    }))
}

#[cfg(test)]
mod tests {
    use super::{ExportManifest, EXPORT_VERSION};

    #[test]
    fn manifest_version_is_explicit_and_round_trips() {
        let manifest = ExportManifest {
            version: EXPORT_VERSION,
            account: super::ExportAccount {
                public_key: "pk".into(),
                enc_private_keys: "keys".into(),
                kdf_params: "kdf".into(),
                recovery_blob: "recovery".into(),
            },
            projects: Vec::new(),
            environments: Vec::new(),
            not_shared_environment_ids: Vec::new(),
            omitted_environment_count: 0,
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        let decoded: ExportManifest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.version, 1);
    }
}
