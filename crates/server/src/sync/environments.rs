//! Environment create + list, nested under a project. Each environment has its own wrapped vault
//! key and a monotonic `revision` (the sync ETag).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::sync::access;
use crate::sync::{validate_id, MAX_ENC_KEY, MAX_ENC_NAME};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/projects/{project_id}/environments",
        get(list_environments).post(create_environment),
    )
}

#[derive(Deserialize)]
struct CreateEnvironment {
    id: String,
    enc_name: String,
    enc_vault_key: String,
}

#[derive(Serialize)]
struct EnvironmentView {
    id: String,
    enc_name: String,
    /// The CALLER's own vault-key grant for this environment, or `null` if they hold none (they can
    /// see the environment exists via org access, but can't decrypt it until someone shares it).
    enc_vault_key: Option<String>,
    revision: i64,
}

/// `POST /projects/{project_id}/environments` — create an environment. Creating one is a structural
/// change, so the caller must be the personal owner or an admin+ of the project's org. Idempotent on
/// re-create of the same id; 409 if the id is taken under a different project.
async fn create_environment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<String>,
    Json(body): Json<CreateEnvironment>,
) -> Result<StatusCode> {
    validate_id(&body.id, "id")?;
    let enc_name = encoding::decode(&body.enc_name, "enc_name", MAX_ENC_NAME)?;
    let enc_vault_key = encoding::decode(&body.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?;
    let access = access::project_access(&state, &project_id, &user.user_id).await?;
    if !access.can_manage_structure() {
        return Err(Error::Forbidden(
            "must be an admin or owner to create an environment".into(),
        ));
    }

    // The environment and the creator's vault-key grant land together (the caller sealed
    // `enc_vault_key` to their own public key). The grant row is the only storage — there is no
    // inline copy on the environment row.
    let created = {
        let mut tx = state.pool.begin().await?;
        let created: Option<String> = sqlx::query_scalar(
            "INSERT INTO environments (id, project_id, enc_name) \
             VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING RETURNING id",
        )
        .bind(&body.id)
        .bind(&project_id)
        .bind(&enc_name)
        .fetch_optional(&mut *tx)
        .await?;

        if created.is_some() {
            sqlx::query(
                "INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by) \
                 VALUES ($1, $2, $3, $2) ON CONFLICT (env_id, user_id) DO NOTHING",
            )
            .bind(&body.id)
            .bind(&user.user_id)
            .bind(&enc_vault_key)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            true
        } else {
            false
        }
    };

    if created {
        return Ok(StatusCode::CREATED);
    }

    // Conflict: idempotent only if the existing environment already sits under this project.
    let existing_project: Option<String> =
        sqlx::query_scalar("SELECT project_id FROM environments WHERE id = $1")
            .bind(&body.id)
            .fetch_optional(&state.pool)
            .await?;
    match existing_project {
        Some(p) if p == project_id => Ok(StatusCode::OK),
        _ => Err(Error::Conflict("environment id already in use".into())),
    }
}

/// `GET /projects/{project_id}/environments` — list environments in a project the caller can
/// reach, each with the caller's OWN vault-key grant (null when they hold none).
async fn list_environments(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<String>,
) -> Result<Json<Vec<EnvironmentView>>> {
    access::project_access(&state, &project_id, &user.user_id).await?;

    /// Listing row: `(id, enc_name, caller's grant, revision)`.
    type EnvRow = (String, Vec<u8>, Option<Vec<u8>>, i64);
    let rows: Vec<EnvRow> = sqlx::query_as(
        "SELECT e.id, e.enc_name, eg.enc_vault_key, e.revision FROM environments e \
         LEFT JOIN environment_grants eg ON eg.env_id = e.id AND eg.user_id = $2 \
         WHERE e.project_id = $1 ORDER BY e.id",
    )
    .bind(&project_id)
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(id, enc_name, enc_vault_key, revision)| EnvironmentView {
                id,
                enc_name: encoding::encode(&enc_name),
                enc_vault_key: enc_vault_key.as_deref().map(encoding::encode),
                revision,
            })
            .collect(),
    ))
}
