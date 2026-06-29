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
    enc_vault_key: String,
    revision: i64,
}

/// Return an error unless `project_id` exists and is owned by the caller.
async fn assert_project_owned(state: &AppState, project_id: &str, user_id: &str) -> Result<()> {
    let owned: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM projects WHERE id = $1 AND owner_id = $2")
            .bind(project_id)
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    owned
        .map(|_| ())
        .ok_or_else(|| Error::NotFound("project not found".into()))
}

/// `POST /projects/{project_id}/environments` — create an environment in a project the caller owns.
/// Idempotent on re-create of one's own id; 409 if the id is taken under a different environment.
async fn create_environment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<String>,
    Json(body): Json<CreateEnvironment>,
) -> Result<StatusCode> {
    validate_id(&body.id, "id")?;
    let enc_name = encoding::decode(&body.enc_name, "enc_name", MAX_ENC_NAME)?;
    let enc_vault_key = encoding::decode(&body.enc_vault_key, "enc_vault_key", MAX_ENC_KEY)?;
    assert_project_owned(&state, &project_id, &user.user_id).await?;

    let created: Option<String> = sqlx::query_scalar(
        "INSERT INTO environments (id, project_id, enc_name, enc_vault_key) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(&body.id)
    .bind(&project_id)
    .bind(&enc_name)
    .bind(&enc_vault_key)
    .fetch_optional(&state.pool)
    .await?;

    if created.is_some() {
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

/// `GET /projects/{project_id}/environments` — list environments in a project the caller owns.
async fn list_environments(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<String>,
) -> Result<Json<Vec<EnvironmentView>>> {
    assert_project_owned(&state, &project_id, &user.user_id).await?;

    let rows: Vec<(String, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT id, enc_name, enc_vault_key, revision FROM environments \
         WHERE project_id = $1 ORDER BY id",
    )
    .bind(&project_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(id, enc_name, enc_vault_key, revision)| EnvironmentView {
                id,
                enc_name: encoding::encode(&enc_name),
                enc_vault_key: encoding::encode(&enc_vault_key),
                revision,
            })
            .collect(),
    ))
}
