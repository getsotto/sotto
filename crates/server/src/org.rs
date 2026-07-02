//! Organizations, memberships, and roles — the team-RBAC substrate.
//!
//! Authority lives in `organization_memberships`: a caller's [`Role`] in an org decides what they
//! may do. Managing members needs `admin` or higher; only an `owner` may grant/modify the `owner`
//! role or delete the org; an org can never be left with zero owners. To avoid leaking which orgs
//! exist, a non-member is answered with `404` (not `403`) — `403` is reserved for a member who is
//! in the org but lacks the role for the action.
//!
//! This is the authorization layer. Resource access is resolved from membership in
//! [`crate::sync::access`], and the crypto grants that let members actually *decrypt* live in
//! [`crate::sync::grants`] — cryptography is not access control, so both layers check. `enc_name`
//! here is server-opaque ciphertext, exactly like `projects.enc_name`.

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
use crate::state::AppState;
use crate::sync::{validate_id, MAX_ENC_NAME};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs", get(list_orgs).post(create_org))
        .route("/orgs/{org_id}", axum::routing::delete(delete_org))
        .route("/orgs/{org_id}/members", get(list_members).post(add_member))
        .route(
            "/orgs/{org_id}/members/{user_id}",
            post(update_member).delete(remove_member),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/grants",
            get(member_env_grants),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/org-key",
            post(grant_org_key),
        )
        .route("/orgs/{org_id}/invites", post(invite_member))
}

/// A member's role in an organization. Ordered `member < admin < owner`; the numeric [`Role::rank`]
/// backs every capability check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    /// Parse a role from its stored text. Unknown values mean the DB and this enum disagree — a bug,
    /// not client input (a `CHECK` constraint keeps the column to the three known values).
    pub(crate) fn from_db(s: &str) -> Result<Role> {
        match s {
            "owner" => Ok(Role::Owner),
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            other => Err(Error::Config(format!(
                "unknown membership role in db: {other}"
            ))),
        }
    }

    fn rank(self) -> u8 {
        match self {
            Role::Owner => 2,
            Role::Admin => 1,
            Role::Member => 0,
        }
    }

    /// Whether this role is at least as privileged as `other` (the ordering behind capability
    /// checks, both here and in the sync layer's resource access).
    pub(crate) fn is_at_least(self, other: Role) -> bool {
        self.rank() >= other.rank()
    }

    /// Whether this role may add, update, or remove members (owners and admins may; members may not).
    pub(crate) fn can_manage_members(self) -> bool {
        self.is_at_least(Role::Admin)
    }
}

/// The caller's role in `org_id` if they are a member, else `None` — a non-erroring lookup for the
/// sync layer's access checks (which turn "not a member" into a resource `404`, not an org error).
pub(crate) async fn role_of(
    pool: &sqlx::PgPool,
    org_id: &str,
    user_id: &str,
) -> Result<Option<Role>> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    role.map(|r| Role::from_db(&r)).transpose()
}

// --- request/response shapes -------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateOrg {
    id: String,
    /// The org name, encrypted under the org key (client-generated).
    enc_name: String,
    /// The org key sealed to the creator's public key — their own copy, stored on their membership.
    enc_org_key: String,
}

#[derive(Serialize)]
struct OrgView {
    id: String,
    enc_name: String,
    /// The caller's own role in this org.
    role: Role,
    /// The org key sealed to the caller (base64), or `null` if they haven't been granted it —
    /// without it, clients show record ids instead of names.
    enc_org_key: Option<String>,
}

#[derive(Deserialize)]
struct GrantOrgKey {
    /// The org key sealed to the target member's public key.
    enc_org_key: String,
}

#[derive(Deserialize)]
struct AddMember {
    user_id: String,
    role: Role,
}

#[derive(Deserialize)]
struct UpdateMember {
    role: Role,
}

#[derive(Serialize)]
struct MemberView {
    user_id: String,
    role: Role,
    /// The member's X25519 public key (base64), or `null` if they haven't set up their account yet.
    /// Sharing an environment seals its vault key to this.
    public_key: Option<String>,
}

#[derive(Deserialize)]
struct Invite {
    email: String,
}

#[derive(Serialize)]
struct InviteResult {
    /// The resolved user's id, now a `member` of the org.
    user_id: String,
    /// Their public key (base64) if set — lets the inviter seal env grants immediately.
    public_key: Option<String>,
}

// --- authorization helpers ---------------------------------------------------------------------

/// The caller's role in `org_id`, or `404` if they are not a member (which also covers "org does
/// not exist" — the two are indistinguishable to a non-member on purpose).
async fn caller_role(state: &AppState, org_id: &str, user_id: &str) -> Result<Role> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    match role {
        Some(r) => Role::from_db(&r),
        None => Err(Error::NotFound("organization not found".into())),
    }
}

/// The current role of `target` in `org_id`, or `404` if they are not a member. Generic over the
/// executor so it reads either from the pool or from within a transaction holding the owner lock.
async fn member_role<'e, E>(exec: E, org_id: &str, target: &str) -> Result<Role>
where
    E: sqlx::PgExecutor<'e>,
{
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(target)
    .fetch_optional(exec)
    .await?;
    match role {
        Some(r) => Role::from_db(&r),
        None => Err(Error::NotFound("member not found".into())),
    }
}

/// Lock every owner row of `org_id` for the rest of `tx`, returning how many there are. Holding
/// these row locks serializes concurrent demotions and removals: without it two owners can each
/// observe "more than one owner", both proceed, and leave the org with zero owners.
async fn lock_owner_count(tx: &mut Transaction<'_, Postgres>, org_id: &str) -> Result<usize> {
    let owners: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM organization_memberships \
         WHERE org_id = $1 AND role = 'owner' FOR UPDATE",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(owners.len())
}

// --- handlers ----------------------------------------------------------------------------------

/// `POST /orgs` — create an org; the creator becomes its first `owner`. Idempotent: re-creating one
/// the caller already owns returns 200; an id already in use by another org returns 409.
async fn create_org(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateOrg>,
) -> Result<StatusCode> {
    validate_id(&body.id, "id")?;
    let enc_name = encoding::decode(&body.enc_name, "enc_name", MAX_ENC_NAME)?;
    let enc_org_key = encoding::decode(&body.enc_org_key, "enc_org_key", MAX_ENC_NAME)?;

    // Org row and the creator's owner membership (carrying their sealed org key) must land
    // together, or not at all.
    let created = {
        let mut tx = state.pool.begin().await?;
        let created: Option<String> = sqlx::query_scalar(
            "INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO NOTHING RETURNING id",
        )
        .bind(&body.id)
        .bind(&enc_name)
        .bind(&user.user_id)
        .fetch_optional(&mut *tx)
        .await?;

        if created.is_some() {
            sqlx::query(
                "INSERT INTO organization_memberships (org_id, user_id, role, enc_org_key) \
                 VALUES ($1, $2, 'owner', $3)",
            )
            .bind(&body.id)
            .bind(&user.user_id)
            .bind(&enc_org_key)
            .execute(&mut *tx)
            .await?;
            audit::record_tx(
                &mut tx,
                &body.id,
                &user.user_id,
                "org.created",
                audit::Context::default(),
            )
            .await?;
            tx.commit().await?;
            true
        } else {
            // Leave the existing org untouched; the tx rolls back on drop.
            false
        }
    };

    if created {
        return Ok(StatusCode::CREATED);
    }

    // The id is taken: succeed idempotently only if the caller already owns that org.
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2",
    )
    .bind(&body.id)
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await?;
    match existing.as_deref() {
        Some("owner") => Ok(StatusCode::OK),
        _ => Err(Error::Conflict("organization id already in use".into())),
    }
}

/// Org listing row: `(id, enc_name, role, enc_org_key)`.
type OrgRow = (String, Vec<u8>, String, Option<Vec<u8>>);

/// `GET /orgs` — the orgs the caller belongs to, each with the caller's own role and (when granted)
/// their sealed copy of the org key.
async fn list_orgs(State(state): State<AppState>, user: AuthUser) -> Result<Json<Vec<OrgView>>> {
    let rows: Vec<OrgRow> = sqlx::query_as(
        "SELECT o.id, o.enc_name, m.role, m.enc_org_key \
         FROM organizations o JOIN organization_memberships m ON o.id = m.org_id \
         WHERE m.user_id = $1 ORDER BY o.id",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await?;

    let orgs = rows
        .into_iter()
        .map(|(id, enc_name, role, enc_org_key)| {
            Ok(OrgView {
                id,
                enc_name: encoding::encode(&enc_name),
                role: Role::from_db(&role)?,
                enc_org_key: enc_org_key.as_deref().map(encoding::encode),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(orgs))
}

/// `POST /orgs/{org_id}/members/{user_id}/org-key` — store (or replace) a member's sealed copy of
/// the org key (admin+). Invites and env-sharing upsert this so members can read display names;
/// it also re-grants a member whose account reset NULLed their copy.
async fn grant_org_key(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(String, String)>,
    Json(body): Json<GrantOrgKey>,
) -> Result<StatusCode> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to grant the org key".into(),
        ));
    }
    let enc_org_key = encoding::decode(&body.enc_org_key, "enc_org_key", MAX_ENC_NAME)?;
    // Grant and its audit event commit together, so a failed audit can't leave an un-logged change.
    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE organization_memberships SET enc_org_key = $3 WHERE org_id = $1 AND user_id = $2",
    )
    .bind(&org_id)
    .bind(&target)
    .bind(&enc_org_key)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(Error::NotFound("member not found".into()));
    }
    audit::record_tx(
        &mut tx,
        &org_id,
        &user.user_id,
        "member.org_key_granted",
        audit::Context {
            target: Some(&target),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// `DELETE /orgs/{org_id}` — delete an org (owner only). Cascades to its memberships.
async fn delete_org(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<StatusCode> {
    if caller_role(&state, &org_id, &user.user_id).await? != Role::Owner {
        return Err(Error::Forbidden(
            "only an owner can delete the organization".into(),
        ));
    }
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /orgs/{org_id}/members` — list members (any member of the org may).
async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<Vec<MemberView>>> {
    caller_role(&state, &org_id, &user.user_id).await?;

    let rows: Vec<(String, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT m.user_id, m.role, u.public_key \
         FROM organization_memberships m JOIN users u ON m.user_id = u.id \
         WHERE m.org_id = $1 ORDER BY m.user_id",
    )
    .bind(&org_id)
    .fetch_all(&state.pool)
    .await?;

    let members = rows
        .into_iter()
        .map(|(user_id, role, public_key)| {
            Ok(MemberView {
                user_id,
                role: Role::from_db(&role)?,
                public_key: public_key.as_deref().map(encoding::encode),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(members))
}

/// `POST /orgs/{org_id}/members` — add an existing user (admin+). Granting `owner` requires the
/// caller to be an owner. 409 if already a member; 404 if the user does not exist.
async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(body): Json<AddMember>,
) -> Result<StatusCode> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to add members".into(),
        ));
    }
    if body.role == Role::Owner && caller != Role::Owner {
        return Err(Error::Forbidden(
            "only an owner can grant the owner role".into(),
        ));
    }

    let inserted: std::result::Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (org_id, user_id) DO NOTHING RETURNING user_id",
    )
    .bind(&org_id)
    .bind(&body.user_id)
    .bind(body.role.as_str())
    .fetch_optional(&state.pool)
    .await;

    match inserted {
        Ok(Some(_)) => {
            audit::record(
                &state.pool,
                &org_id,
                &user.user_id,
                "member.added",
                audit::Context {
                    target: Some(&body.user_id),
                    detail: Some(body.role.as_str()),
                    ..Default::default()
                },
            )
            .await?;
            Ok(StatusCode::CREATED)
        }
        // No row inserted, no error: the (org, user) pair already existed.
        Ok(None) => Err(Error::Conflict(
            "user is already a member; use update to change their role".into(),
        )),
        // The user_id has no matching users row.
        Err(e) if is_user_fk_violation(&e) => Err(Error::NotFound("user not found".into())),
        Err(e) => Err(e.into()),
    }
}

/// `POST /orgs/{org_id}/invites` — invite an existing Sotto user by email (admin+). The email must
/// resolve to exactly one existing user, who is added as a `member`; returns their id + public key
/// so the inviter can seal env grants right away. 404 if no such user, 409 if ambiguous or already a
/// member. (Existing-users-only: there is no pending-invite/onboarding flow yet.)
async fn invite_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(body): Json<Invite>,
) -> Result<Json<InviteResult>> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to invite members".into(),
        ));
    }

    // Resolve the email to exactly one existing user (email is not unique in the schema, so guard
    // against the ambiguous case rather than silently picking one).
    let matches: Vec<(String, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT id, public_key FROM users WHERE email = $1")
            .bind(&body.email)
            .fetch_all(&state.pool)
            .await?;
    let (target_id, public_key) = match matches.as_slice() {
        [] => return Err(Error::NotFound("no Sotto user with that email".into())),
        [only] => only.clone(),
        _ => return Err(Error::Conflict("multiple users share that email".into())),
    };

    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'member') \
         ON CONFLICT (org_id, user_id) DO NOTHING RETURNING user_id",
    )
    .bind(&org_id)
    .bind(&target_id)
    .fetch_optional(&state.pool)
    .await?;
    if inserted.is_none() {
        return Err(Error::Conflict("user is already a member".into()));
    }
    audit::record(
        &state.pool,
        &org_id,
        &user.user_id,
        "member.invited",
        audit::Context {
            target: Some(&target_id),
            ..Default::default()
        },
    )
    .await?;

    Ok(Json(InviteResult {
        user_id: target_id,
        public_key: public_key.as_deref().map(encoding::encode),
    }))
}

#[derive(Serialize)]
struct EnvGrantRef {
    env_id: String,
}

/// `GET /orgs/{org_id}/members/{user_id}/grants` — the ids of this org's environments that `user_id`
/// currently holds a grant to (admin+). The removal flow uses this to enumerate what to re-key.
async fn member_env_grants(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(String, String)>,
) -> Result<Json<Vec<EnvGrantRef>>> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to list a member's grants".into(),
        ));
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT eg.env_id FROM environment_grants eg \
         JOIN environments e ON eg.env_id = e.id JOIN projects p ON e.project_id = p.id \
         WHERE p.org_id = $1 AND eg.user_id = $2 ORDER BY eg.env_id",
    )
    .bind(&org_id)
    .bind(&target)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|env_id| EnvGrantRef { env_id })
            .collect(),
    ))
}

/// `POST /orgs/{org_id}/members/{user_id}` — change a member's role (admin+). Changing to or from
/// `owner` requires the caller to be an owner; the last owner cannot be demoted.
async fn update_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(String, String)>,
    Json(body): Json<UpdateMember>,
) -> Result<StatusCode> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to change roles".into(),
        ));
    }
    // Lock the org's owner set for the rest of the transaction so the last-owner guard and the
    // write commit atomically; two owners demoting each other concurrently serialize here.
    let mut tx = state.pool.begin().await?;
    let owners = lock_owner_count(&mut tx, &org_id).await?;
    let current = member_role(&mut *tx, &org_id, &target).await?;

    if (body.role == Role::Owner || current == Role::Owner) && caller != Role::Owner {
        return Err(Error::Forbidden(
            "only an owner can change the owner role".into(),
        ));
    }
    if current == Role::Owner && body.role != Role::Owner && owners == 1 {
        return Err(Error::Conflict("cannot demote the last owner".into()));
    }

    sqlx::query("UPDATE organization_memberships SET role = $3 WHERE org_id = $1 AND user_id = $2")
        .bind(&org_id)
        .bind(&target)
        .bind(body.role.as_str())
        .execute(&mut *tx)
        .await?;
    audit::record_tx(
        &mut tx,
        &org_id,
        &user.user_id,
        "member.role_changed",
        audit::Context {
            target: Some(&target),
            detail: Some(body.role.as_str()),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// `DELETE /orgs/{org_id}/members/{user_id}` — remove a member (admin+). Removing an owner requires
/// the caller to be an owner; the last owner cannot be removed.
async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(String, String)>,
) -> Result<StatusCode> {
    let caller = caller_role(&state, &org_id, &user.user_id).await?;
    if !caller.can_manage_members() {
        return Err(Error::Forbidden(
            "must be an admin or owner to remove members".into(),
        ));
    }
    // Lock the org's owner set for the rest of the transaction so the last-owner guard and the
    // delete commit atomically; two owners removing each other concurrently serialize here.
    let mut tx = state.pool.begin().await?;
    let owners = lock_owner_count(&mut tx, &org_id).await?;
    let current = member_role(&mut *tx, &org_id, &target).await?;

    if current == Role::Owner {
        if caller != Role::Owner {
            return Err(Error::Forbidden("only an owner can remove an owner".into()));
        }
        if owners == 1 {
            return Err(Error::Conflict("cannot remove the last owner".into()));
        }
    }

    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1 AND user_id = $2")
        .bind(&org_id)
        .bind(&target)
        .execute(&mut *tx)
        .await?;
    audit::record_tx(
        &mut tx,
        &org_id,
        &user.user_id,
        "member.removed",
        audit::Context {
            target: Some(&target),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// True only for the `user_id` foreign-key violation — the referenced user does not exist. Scoped
/// to that one constraint by name so an unrelated FK failure (e.g. a racing org deletion breaking
/// the `org_id` reference) isn't misreported as "user not found".
fn is_user_fk_violation(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db)
            if db.code().as_deref() == Some("23503")
                && db.constraint() == Some("organization_memberships_user_fk")
    )
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn role_ordering_gates_management() {
        assert!(Role::Owner.can_manage_members());
        assert!(Role::Admin.can_manage_members());
        assert!(!Role::Member.can_manage_members());
        assert!(Role::Owner.rank() > Role::Admin.rank());
        assert!(Role::Admin.rank() > Role::Member.rank());
    }

    #[test]
    fn role_text_round_trips() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(Role::from_db(role.as_str()).unwrap(), role);
        }
        assert!(Role::from_db("root").is_err());
    }
}
