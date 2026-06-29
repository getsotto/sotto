//! Project create + list. A project groups environments; it is owned by the user who creates it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::sync::{validate_id, MAX_ENC_NAME};

pub fn router() -> Router<AppState> {
    Router::new().route("/projects", get(list_projects).post(create_project))
}

#[derive(Deserialize)]
struct CreateProject {
    id: String,
    enc_name: String,
}

#[derive(Serialize)]
struct ProjectView {
    id: String,
    enc_name: String,
}

/// `POST /projects` — create a project owned by the caller. Idempotent: re-creating one the caller
/// already owns returns 200; an id already owned by someone else returns 409.
async fn create_project(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateProject>,
) -> Result<StatusCode> {
    validate_id(&body.id, "id")?;
    let enc_name = encoding::decode(&body.enc_name, "enc_name", MAX_ENC_NAME)?;

    let created: Option<String> = sqlx::query_scalar(
        "INSERT INTO projects (id, owner_id, enc_name) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(&body.id)
    .bind(&user.user_id)
    .bind(&enc_name)
    .fetch_optional(&state.pool)
    .await?;

    if created.is_some() {
        return Ok(StatusCode::CREATED);
    }

    // Conflict: succeed idempotently only if the caller already owns this id.
    let owner: Option<String> = sqlx::query_scalar("SELECT owner_id FROM projects WHERE id = $1")
        .bind(&body.id)
        .fetch_optional(&state.pool)
        .await?;
    match owner {
        Some(owner_id) if owner_id == user.user_id => Ok(StatusCode::OK),
        _ => Err(Error::Conflict("project id already in use".into())),
    }
}

/// `GET /projects` — list the caller's projects.
async fn list_projects(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ProjectView>>> {
    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT id, enc_name FROM projects WHERE owner_id = $1 ORDER BY id")
            .bind(&user.user_id)
            .fetch_all(&state.pool)
            .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(id, enc_name)| ProjectView {
                id,
                enc_name: encoding::encode(&enc_name),
            })
            .collect(),
    ))
}
