//! Project create + list. A project groups environments; it is owned by the user who creates it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::encoding;
use crate::error::{Error, Result};
use crate::org::{self, Role};
use crate::state::AppState;
use crate::sync::access;
use crate::sync::{validate_id, MAX_ENC_NAME};

pub fn router() -> Router<AppState> {
    Router::new().route("/projects", get(list_projects).post(create_project))
}

#[derive(Deserialize)]
struct CreateProject {
    id: String,
    enc_name: String,
    /// Owning organization; omitted creates a personal project owned by the caller.
    #[serde(default)]
    org_id: Option<String>,
}

#[derive(Serialize)]
struct ProjectView {
    id: String,
    enc_name: String,
    /// Owning organization, or `null` for a personal project. Structural metadata (not secret);
    /// the web client uses it to offer team actions (share, members) on org projects.
    org_id: Option<String>,
}

/// `POST /projects` — create a project. Personal (no `org_id`) is owned by the caller; an org
/// project requires the caller to be an admin+ of that org. Idempotent: re-creating one of the same
/// shape the caller can reach returns 200; an id already in use otherwise returns 409.
async fn create_project(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateProject>,
) -> Result<StatusCode> {
    validate_id(&body.id, "id")?;
    let enc_name = encoding::decode(&body.enc_name, "enc_name", MAX_ENC_NAME)?;

    // Creating a project inside an org is a structural change: admin+ (404 hides non-membership).
    if let Some(org_id) = &body.org_id {
        match org::role_of(&state.pool, org_id, &user.user_id).await? {
            Some(role) if role.is_at_least(Role::Admin) => {}
            Some(_) => {
                return Err(Error::Forbidden(
                    "must be an admin or owner to create a project in this organization".into(),
                ))
            }
            None => return Err(Error::NotFound("organization not found".into())),
        }
    }

    let created: Option<String> = sqlx::query_scalar(
        "INSERT INTO projects (id, owner_id, org_id, enc_name) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(&body.id)
    .bind(&user.user_id)
    .bind(&body.org_id)
    .bind(&enc_name)
    .fetch_optional(&state.pool)
    .await?;

    if created.is_some() {
        return Ok(StatusCode::CREATED);
    }

    // Conflict: succeed idempotently only if the existing project is the same shape (same owning
    // org) and the caller can already reach it; otherwise the id is taken.
    let existing: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT owner_id, org_id FROM projects WHERE id = $1")
            .bind(&body.id)
            .fetch_optional(&state.pool)
            .await?;
    let same_shape = matches!(&existing, Some((_, org_id)) if *org_id == body.org_id);
    if same_shape
        && access::project_access(&state, &body.id, &user.user_id)
            .await
            .is_ok()
    {
        Ok(StatusCode::OK)
    } else {
        Err(Error::Conflict("project id already in use".into()))
    }
}

/// `GET /projects` — list the projects the caller can reach: their own personal projects plus every
/// project of an org they belong to.
async fn list_projects(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ProjectView>>> {
    let rows: Vec<(String, Vec<u8>, Option<String>)> = sqlx::query_as(
        "SELECT id, enc_name, org_id FROM projects p \
         WHERE (p.org_id IS NULL AND p.owner_id = $1) \
            OR (p.org_id IS NOT NULL AND EXISTS ( \
                   SELECT 1 FROM organization_memberships m \
                   WHERE m.org_id = p.org_id AND m.user_id = $1)) \
         ORDER BY id",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(id, enc_name, org_id)| ProjectView {
                id,
                enc_name: encoding::encode(&enc_name),
                org_id,
            })
            .collect(),
    ))
}
