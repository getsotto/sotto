//! Team operations over the sync API: organisations, invites, and environment sharing.
//!
//! Every org has an **org key** - a symmetric key sealed grant-style to each member's public key
//! (stored on their membership row, server-opaque). It encrypts org/project/environment *display
//! names* for org resources, so every member reads real names instead of record-id fallbacks.
//! Metadata only: secret names/values are protected by the per-environment vault keys, which
//! rotate on member removal. The org key does not rotate - a removed member remembering display
//! names is an accepted, documented leak.
//!
//! Sharing an environment opens the caller's own vault-key grant, reseals the vault key to a
//! member's public key, and uploads that grant - keys never leave the process, and the server only
//! ever holds sealed blobs. Invite and share flows also upsert the member's org-key copy, so
//! whoever can decrypt an environment can also read its name.

use sotto_core::{names, vault, wrap};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::store::Store;

use super::api::{
    b64decode, b64encode, DataKeyEntry, GrantEntry, HistoryKeyEntry, Invited, MachineGrantEntry,
    MemberInfo, NewOrg, RotateRequest, SyncApi,
};

/// Decode a base64 X25519 public key into its fixed-size array.
fn decode_public_key(b64: &str) -> Result<[u8; wrap::PUBLIC_KEY_LEN]> {
    b64decode(b64)?.try_into().map_err(|_| Error::Crypto)
}

/// Re-snapshot + retry budget when a concurrent write bumps the revision during a rotation.
const ROTATE_ATTEMPTS: usize = 5;

/// An organisation with its decrypted name and the caller's role in it.
pub struct OrgListing {
    pub id: String,
    pub name: String,
    pub role: String,
}

/// Create an organisation (the caller becomes its owner); returns the new org id. Generates the
/// org key, encrypts the name under it, and seals the creator's own copy to their public key.
pub fn create_org(api: &dyn SyncApi, keypair: &wrap::Keypair, name: &str) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let mut org_key = vault::generate_vault_key();
    let enc_name = names::encrypt_org_name(&org_key, &id, name.as_bytes());
    let enc_org_key = vault::grant_vault_key(&keypair.public, &org_key)?;
    org_key.zeroize();
    api.create_org(&NewOrg {
        id: id.clone(),
        enc_name: b64encode(&enc_name),
        enc_org_key: b64encode(&enc_org_key),
    })?;
    Ok(id)
}

/// The caller's copy of an org's key, or `None` if they haven't been granted one (an org from
/// before org keys existed, or their copy was cleared by an account reset).
pub fn org_key(
    api: &dyn SyncApi,
    keypair: &wrap::Keypair,
    org_id: &str,
) -> Result<Option<[u8; 32]>> {
    for org in api.list_orgs()? {
        if org.id == org_id {
            let Some(enc) = org.enc_org_key else {
                return Ok(None);
            };
            return Ok(Some(vault::open_vault_key(keypair, &b64decode(&enc)?)?));
        }
    }
    Ok(None)
}

/// List the caller's organisations, decrypting each name with its org key (falling back to the id
/// when the caller holds no org key or the name predates org keys).
pub fn list_orgs(api: &dyn SyncApi, keypair: &wrap::Keypair) -> Result<Vec<OrgListing>> {
    let mut listings = Vec::new();
    for org in api.list_orgs()? {
        let name = decrypt_org_listing_name(keypair, &org).unwrap_or_else(|| org.id.clone());
        listings.push(OrgListing {
            id: org.id,
            name,
            role: org.role,
        });
    }
    Ok(listings)
}

/// Best-effort org-name decryption: open the caller's org key and decrypt, `None` on any failure.
fn decrypt_org_listing_name(keypair: &wrap::Keypair, org: &super::api::OrgInfo) -> Option<String> {
    let enc_org_key = b64decode(org.enc_org_key.as_deref()?).ok()?;
    let mut key = vault::open_vault_key(keypair, &enc_org_key).ok()?;
    let name = names::decrypt_org_name(&key, &org.id, &b64decode(&org.enc_name).ok()?).ok();
    key.zeroize();
    String::from_utf8(name?).ok()
}

/// Invite an existing user (by email) into an org as a member, then grant them the org key (sealed
/// to their public key) so display names decrypt for them. The key grant is best-effort: it needs
/// the invitee's public key on file and the caller's own org-key copy; without either, the invite
/// still succeeds and the member sees record ids until re-granted.
pub fn invite(
    api: &dyn SyncApi,
    keypair: &wrap::Keypair,
    org_id: &str,
    email: &str,
) -> Result<Invited> {
    let invited = api.invite_member(org_id, email)?;
    if let Some(public_key_b64) = &invited.public_key {
        if let Some(mut org_key) = org_key(api, keypair, org_id)? {
            let sealed = vault::grant_vault_key(&decode_public_key(public_key_b64)?, &org_key)?;
            org_key.zeroize();
            api.grant_org_key(org_id, &invited.user_id, &b64encode(&sealed))?;
        }
    }
    Ok(invited)
}

/// List an org's members.
pub fn members(api: &dyn SyncApi, org_id: &str) -> Result<Vec<MemberInfo>> {
    api.list_members(org_id)
}

/// Share the config's active environment with an org member: reseal its vault key to the member's
/// public key and upload the grant. Returns the shared environment's id (for a clone hint).
pub fn share_env(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &wrap::Keypair,
    org_id: &str,
    member_user_id: &str,
    config: &Config,
) -> Result<String> {
    let env = store
        .get_environment(&config.project_id, &config.environment)?
        .ok_or_else(|| Error::NotFound(format!("environment `{}`", config.environment)))?;

    let member = members(api, org_id)?
        .into_iter()
        .find(|m| m.user_id == member_user_id)
        .ok_or_else(|| {
            Error::Input(format!(
                "`{member_user_id}` is not a member of this organisation"
            ))
        })?;
    let public_key_b64 = member.public_key.ok_or_else(|| {
        Error::Input(
            "that member hasn't set up their account keys yet - they must log in and \
             run `sotto setup` first"
                .into(),
        )
    })?;
    let public_key = decode_public_key(&public_key_b64)?;

    // Open our own grant to recover the vault key, then reseal it to the member.
    let mut vault_key = vault::open_vault_key(keypair, &env.enc_vault_key)?;
    let grant = vault::grant_vault_key(&public_key, &vault_key)?;
    vault_key.zeroize();

    api.create_grant(&env.id, member_user_id, &b64encode(&grant))?;

    // Whoever can decrypt an environment should also read its display names: upsert the member's
    // org-key copy too (best-effort - without our own copy there is nothing to grant).
    if let Some(mut org_key) = org_key(api, keypair, org_id)? {
        let sealed = vault::grant_vault_key(&public_key, &org_key)?;
        org_key.zeroize();
        api.grant_org_key(org_id, member_user_id, &b64encode(&sealed))?;
    }
    Ok(env.id)
}

/// Clone a shared environment onto this device: fetch our own grant, reconstruct the project + env
/// locally, and pull its secrets. Labels resolve in order: caller-supplied override → the real
/// name decrypted with the org key → a generic fallback. `org_id` - the owning org - is recorded
/// in the config so later pushes match server-side.
#[allow(clippy::too_many_arguments)]
pub fn clone_env(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &wrap::Keypair,
    project_id: &str,
    env_id: &str,
    project_label: Option<&str>,
    env_label: Option<&str>,
    org_id: Option<&str>,
) -> Result<Config> {
    let grant_b64 = api.get_grant(env_id)?.ok_or_else(|| {
        Error::Input("you have not been granted this environment (ask an admin to share it)".into())
    })?;
    let grant = b64decode(&grant_b64)?;
    // Prove the grant opens with our keypair before persisting anything (fail closed on a bad grant).
    vault::open_vault_key(keypair, &grant)?;

    // Resolve display names with the org key when we can (share/invite granted us a copy).
    let env_name = match env_label {
        Some(label) => label.to_string(),
        None => decrypted_env_name(api, keypair, project_id, env_id, org_id)
            .unwrap_or_else(|| "shared".to_string()),
    };
    let project_name = project_label.unwrap_or("shared").to_string();

    if store.get_project(project_id)?.is_none() {
        store.create_project_with_id(project_id, &project_name)?;
    }
    if store.find_environment(env_id)?.is_none() {
        store.create_environment(env_id, project_id, &env_name, &grant)?;
    }

    let config = Config {
        project_id: project_id.to_string(),
        project: project_name,
        environment: env_name,
        org_id: org_id.map(str::to_string),
    };
    super::sync::pull(api, store, &config)?;
    Ok(config)
}

/// Best-effort: decrypt the environment's real display name with the caller's org key.
fn decrypted_env_name(
    api: &dyn SyncApi,
    keypair: &wrap::Keypair,
    project_id: &str,
    env_id: &str,
    org_id: Option<&str>,
) -> Option<String> {
    let mut key = org_key(api, keypair, org_id?).ok()??;
    let name = api
        .list_environments(project_id)
        .ok()?
        .into_iter()
        .find(|e| e.id == env_id)
        .and_then(|e| {
            let ct = b64decode(&e.enc_name).ok()?;
            names::decrypt_env_name(&key, env_id, &ct).ok()
        });
    key.zeroize();
    String::from_utf8(name?).ok()
}

/// Rotate an environment's vault key: rewrap every data key and re-grant the current holders, minus
/// `revoke`. Returns `Some(new_revision)`, or `None` if the caller holds no grant to this env (so
/// can't open it to re-key) - the caller reports that as skipped. Retries when a concurrent write
/// moves the revision.
pub fn rotate_env(
    api: &dyn SyncApi,
    keypair: &wrap::Keypair,
    org_id: &str,
    env_id: &str,
    revoke: Option<&str>,
) -> Result<Option<i64>> {
    let members = api.list_members(org_id)?;
    for _ in 0..ROTATE_ATTEMPTS {
        // Our current grant → the old vault key. Without one, we can't rotate this environment.
        let Some(old_grant) = api.get_grant(env_id)? else {
            return Ok(None);
        };
        let mut old_vault = vault::open_vault_key(keypair, &b64decode(&old_grant)?)?;
        let mut new_vault = vault::generate_vault_key();

        // Rewrap every current secret's data key from the old vault key to the new one.
        let snapshot = api
            .snapshot(env_id, None)?
            .ok_or_else(|| Error::Server("server returned no snapshot".into()))?;
        let mut data_keys = Vec::with_capacity(snapshot.secrets.len());
        for s in &snapshot.secrets {
            let rewrapped = vault::rewrap_data_key(
                &old_vault,
                &new_vault,
                env_id,
                &s.id,
                s.version,
                &b64decode(&s.enc_data_key)?,
            )?;
            data_keys.push(DataKeyEntry {
                secret_id: s.id.clone(),
                enc_data_key: b64encode(&rewrapped),
            });
        }

        // Rewrap every retained history version too, so no version silently dies with the old key
        // (the server rejects a rotation that doesn't cover them all).
        let history = api.list_history(env_id)?;
        let mut history_keys = Vec::with_capacity(history.len());
        for row in &history {
            let rewrapped = vault::rewrap_data_key(
                &old_vault,
                &new_vault,
                env_id,
                &row.secret_id,
                row.version,
                &b64decode(&row.enc_data_key)?,
            )?;
            history_keys.push(HistoryKeyEntry {
                secret_id: row.secret_id.clone(),
                version: row.version,
                enc_data_key: b64encode(&rewrapped),
            });
        }
        old_vault.zeroize();

        // Re-grant every current holder except the revoked one, sealing the new vault key to each.
        let mut grants = Vec::new();
        for holder in api.list_grant_holders(env_id)? {
            if Some(holder.as_str()) == revoke {
                continue;
            }
            let public_key_b64 = members
                .iter()
                .find(|m| m.user_id == holder)
                .and_then(|m| m.public_key.clone())
                .ok_or_else(|| {
                    Error::Input(format!("cannot re-grant `{holder}`: no public key on file"))
                })?;
            grants.push(GrantEntry {
                user_id: holder,
                enc_vault_key: b64encode(&vault::grant_vault_key(
                    &decode_public_key(&public_key_b64)?,
                    &new_vault,
                )?),
            });
        }

        // Re-seal to every active machine token too - the server rejects a rotation that would
        // strand CI on the old key.
        let mut machine_grants = Vec::new();
        for token in api.list_machine_tokens(env_id)? {
            machine_grants.push(MachineGrantEntry {
                token_id: token.token_id,
                enc_vault_key: b64encode(&vault::grant_vault_key(
                    &decode_public_key(&token.public_key)?,
                    &new_vault,
                )?),
            });
        }
        new_vault.zeroize();

        let req = RotateRequest {
            base_revision: snapshot.revision,
            grants,
            data_keys,
            machine_grants,
            history_keys,
        };
        match api.rotate(env_id, &req) {
            Ok(resp) => return Ok(Some(resp.revision)),
            // A concurrent write bumped the revision between our snapshot and rotate - retry.
            Err(Error::Conflict(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::Conflict(
        "rotation kept racing concurrent writes; try again".into(),
    ))
}

/// The outcome of removing a member: which environments were re-keyed, and which were skipped
/// because this caller holds no grant to them (someone who does must rotate those).
pub struct RemovalReport {
    pub rotated: Vec<String>,
    pub skipped: Vec<String>,
}

/// Remove a member from an org, first rotating every environment they could decrypt (dropping their
/// grant) so their cached vault keys can't read future writes, then dropping their membership.
pub fn remove_member(
    api: &dyn SyncApi,
    keypair: &wrap::Keypair,
    org_id: &str,
    user_id: &str,
) -> Result<RemovalReport> {
    let mut rotated = Vec::new();
    let mut skipped = Vec::new();
    for env_id in api.member_env_grants(org_id, user_id)? {
        match rotate_env(api, keypair, org_id, &env_id, Some(user_id))? {
            Some(_) => rotated.push(env_id),
            None => skipped.push(env_id),
        }
    }
    // Finally drop the membership, revoking their API access.
    api.remove_member(org_id, user_id)?;
    Ok(RemovalReport { rotated, skipped })
}

/// Create a machine token for an environment: generate the machine's X25519 keypair locally, open
/// our own grant, re-seal the vault key to the machine, upload the public half, and assemble the
/// `SOTTO_TOKEN` string. The machine's private key never reaches the server.
pub fn create_machine_token(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &wrap::Keypair,
    config: &Config,
    name: &str,
) -> Result<String> {
    let env = store
        .get_environment(&config.project_id, &config.environment)?
        .ok_or_else(|| Error::NotFound(format!("environment `{}`", config.environment)))?;

    let machine = wrap::generate_keypair();
    let mut vault_key = vault::open_vault_key(keypair, &env.enc_vault_key)?;
    let machine_grant = vault::grant_vault_key(&machine.public, &vault_key)?;
    vault_key.zeroize();

    let created = api.create_machine_token(
        &env.id,
        name,
        &b64encode(&machine.public),
        &b64encode(&machine_grant),
    )?;
    Ok(super::machine::assemble_token(
        &created.token,
        &machine.secret,
    ))
}
