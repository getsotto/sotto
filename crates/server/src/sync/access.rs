//! Resolving a caller's access to a project — and thus its environments and secrets.
//!
//! A project is either *personal* (`org_id IS NULL`, governed by `owner_id`) or *org-owned*
//! (governed by the caller's membership role). A successful resolve means the caller may read and
//! write secrets — any member is a collaborator. Structural changes (creating projects and
//! environments) additionally require [`ProjectAccess::can_manage_structure`] (admin+ or the
//! personal owner). A caller with no access is answered `404`, never leaking that the resource
//! exists.

use crate::error::{Error, Result};
use crate::org::{self, Role};
use crate::state::AppState;

/// A resolved grant of access to a project. Merely holding one authorizes reads and secret writes;
/// the methods gate the more privileged operations.
pub(crate) struct ProjectAccess {
    /// The caller is the personal owner of a non-org project.
    is_owner: bool,
    /// The caller's role in the owning org, for an org project.
    org_role: Option<Role>,
    /// The owning org, for an org project (carried so callers — e.g. audit logging — don't re-query).
    org_id: Option<String>,
}

impl ProjectAccess {
    /// Whether the caller may make structural changes (create environments, or projects in the org).
    /// Reads and secret writes need no such check — a successful resolve already grants them.
    pub(crate) fn can_manage_structure(&self) -> bool {
        self.is_owner || self.org_role.is_some_and(|r| r.is_at_least(Role::Admin))
    }

    /// The owning organization's id, or `None` for a personal project.
    pub(crate) fn org_id(&self) -> Option<&str> {
        self.org_id.as_deref()
    }
}

/// Resolve the caller's access to `project_id`, or `404` if it does not exist or they cannot reach
/// it (the two are indistinguishable to an outsider on purpose).
pub(crate) async fn project_access(
    state: &AppState,
    project_id: &str,
    user_id: &str,
) -> Result<ProjectAccess> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT owner_id, org_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await?;
    let (owner_id, org_id) = row.ok_or_else(|| Error::NotFound("project not found".into()))?;

    match org_id {
        // Personal project: only its owner may reach it.
        None if owner_id == user_id => Ok(ProjectAccess {
            is_owner: true,
            org_role: None,
            org_id: None,
        }),
        None => Err(Error::NotFound("project not found".into())),
        // Org project: authority is the caller's membership role, not `owner_id`.
        Some(org) => match org::role_of(&state.pool, &org, user_id).await? {
            Some(role) => Ok(ProjectAccess {
                is_owner: false,
                org_role: Some(role),
                org_id: Some(org),
            }),
            None => Err(Error::NotFound("project not found".into())),
        },
    }
}

/// Resolve the caller's access to the project owning `env_id`; returns `(project_id, access)` or
/// `404` if the environment does not exist or the caller cannot reach its project.
pub(crate) async fn env_access(
    state: &AppState,
    env_id: &str,
    user_id: &str,
) -> Result<(String, ProjectAccess)> {
    let project_id: Option<String> =
        sqlx::query_scalar("SELECT project_id FROM environments WHERE id = $1")
            .bind(env_id)
            .fetch_optional(&state.pool)
            .await?;
    let project_id = project_id.ok_or_else(|| Error::NotFound("environment not found".into()))?;
    let access = project_access(state, &project_id, user_id).await?;
    Ok((project_id, access))
}
