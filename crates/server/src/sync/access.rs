//! Resolving a caller's access to a project - and thus its environments and secrets.
//!
//! A project is either *personal* (`org_id IS NULL`, governed by `owner_id`) or *org-owned*
//! (governed by the caller's membership role). A successful resolve always permits reads; active
//! organisation members may also write secrets, while retention permits reads only. Structural
//! changes (creating projects and environments) additionally require
//! [`ProjectAccess::can_manage_structure`] (admin+ or the personal owner). A caller with no access
//! is answered `404`, never leaking that the resource exists.

use crate::error::{Error, Result};
use crate::org::{self, LifecycleState, Role};
use crate::state::AppState;
use sqlx::{Postgres, Transaction};

/// A resolved grant of access to a project. Holding one authorises reads; active organisation
/// members may write secrets, and the methods gate lifecycle and structural permissions.
pub(crate) struct ProjectAccess {
    /// The caller is the personal owner of a non-org project.
    is_owner: bool,
    /// The caller's role in the owning org, for an org project.
    org_role: Option<Role>,
    /// The owning org, for an org project (carried so callers - e.g. audit logging - don't re-query).
    org_id: Option<String>,
    /// The owning organisation's lifecycle state, when this is an org project.
    org_lifecycle: Option<LifecycleState>,
    /// The caller whose membership was resolved.
    user_id: String,
}

impl ProjectAccess {
    /// Whether the caller may make structural changes (create environments, or projects in the org).
    /// Reads and secret writes need no such check - a successful resolve already grants them.
    pub(crate) fn can_manage_structure(&self) -> bool {
        self.is_owner || self.org_role.is_some_and(|r| r.is_at_least(Role::Admin))
    }

    /// The owning organisation's id, or `None` for a personal project.
    pub(crate) fn org_id(&self) -> Option<&str> {
        self.org_id.as_deref()
    }

    /// Reject a write to a project or environment while its organisation is being deleted.
    pub(crate) fn require_write(&self) -> Result<()> {
        if let Some(lifecycle) = self.org_lifecycle {
            lifecycle.require_write()?
        }
        Ok(())
    }

    /// Require both a live organisation and the admin/owner role for a structural mutation.
    pub(crate) fn require_manage_structure(&self, message: &str) -> Result<()> {
        self.require_write()?;
        if !self.can_manage_structure() {
            return Err(Error::Forbidden(message.into()));
        }
        Ok(())
    }

    /// Recheck membership and lifecycle while holding the organisation lock for a write.
    pub(crate) async fn require_write_tx(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        if let Some(org_id) = &self.org_id {
            org::access_for_update(tx, org_id, &self.user_id)
                .await?
                .require_write()?;
        }
        Ok(())
    }

    /// Recheck the admin/owner role and lifecycle while holding the organisation lock.
    pub(crate) async fn require_manage_structure_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        message: &str,
    ) -> Result<()> {
        if let Some(org_id) = &self.org_id {
            let access = org::access_for_update(tx, org_id, &self.user_id).await?;
            access.require_write()?;
            if !access.role().is_at_least(Role::Admin) {
                return Err(Error::Forbidden(message.into()));
            }
        } else if !self.can_manage_structure() {
            return Err(Error::Forbidden(message.into()));
        }
        Ok(())
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
            org_lifecycle: None,
            user_id: user_id.to_string(),
        }),
        None => Err(Error::NotFound("project not found".into())),
        // Org project: authority is the caller's membership role, not `owner_id`.
        Some(org) => match org::access(&state.pool, &org, user_id).await {
            Ok(org_access) => Ok(ProjectAccess {
                is_owner: false,
                org_role: Some(org_access.role()),
                org_id: Some(org),
                org_lifecycle: Some(org_access.lifecycle()),
                user_id: user_id.to_string(),
            }),
            Err(Error::NotFound(_)) => Err(Error::NotFound("project not found".into())),
            Err(error) => Err(error),
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
