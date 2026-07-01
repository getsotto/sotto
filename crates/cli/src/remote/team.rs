//! Team operations over the sync API: organizations, invites, and environment sharing.
//!
//! Organization names are encrypted under the master key (like project/environment names), so only
//! server-opaque ciphertext leaves this device. Sharing an environment opens the caller's own
//! vault-key grant, reseals the vault key to a member's public key, and uploads that grant — the
//! vault key never leaves the process, and the server only ever holds sealed grants.
//!
//! NOTE: secret names/values are encrypted under the vault key, so a teammate who is granted an
//! environment can read them. Project/environment *display* names are still under the creator's
//! master key, so a teammate labels a cloned environment locally; shared-readable names await a
//! per-org key (a documented follow-up).

use sotto_core::{aead, vault, wrap};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::store::Store;

use super::api::{b64decode, b64encode, Invited, MemberInfo, NewOrg, OrgInfo, SyncApi};

fn org_name_aad(id: &str) -> String {
    format!("sotto/v1/org-name|id={id}")
}

/// An organization with its decrypted name and the caller's role in it.
pub struct OrgListing {
    pub id: String,
    pub name: String,
    pub role: String,
}

/// Create an organization (the caller becomes its owner); returns the new org id.
pub fn create_org(api: &dyn SyncApi, master: &[u8; 32], name: &str) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let enc_name = aead::seal(master, name.as_bytes(), org_name_aad(&id).as_bytes());
    api.create_org(&NewOrg {
        id: id.clone(),
        enc_name: b64encode(&enc_name),
    })?;
    Ok(id)
}

/// List the caller's organizations, decrypting each name (falling back to the id if a name can't be
/// decrypted — e.g. an org whose name was encrypted under a key this device doesn't hold).
pub fn list_orgs(api: &dyn SyncApi, master: &[u8; 32]) -> Result<Vec<OrgListing>> {
    let mut listings = Vec::new();
    for org in api.list_orgs()? {
        let name = decrypt_org_name(master, &org).unwrap_or_else(|_| org.id.clone());
        listings.push(OrgListing {
            id: org.id,
            name,
            role: org.role,
        });
    }
    Ok(listings)
}

fn decrypt_org_name(master: &[u8; 32], org: &OrgInfo) -> Result<String> {
    let bytes = aead::open(
        master,
        &b64decode(&org.enc_name)?,
        org_name_aad(&org.id).as_bytes(),
    )?;
    String::from_utf8(bytes).map_err(|_| Error::Crypto)
}

/// Invite an existing user (by email) into an org as a member.
pub fn invite(api: &dyn SyncApi, org_id: &str, email: &str) -> Result<Invited> {
    api.invite_member(org_id, email)
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
                "`{member_user_id}` is not a member of this organization"
            ))
        })?;
    let public_key_b64 = member.public_key.ok_or_else(|| {
        Error::Input(
            "that member hasn't set up their account keys yet — they must log in and \
             run `sotto setup` first"
                .into(),
        )
    })?;
    let public_key: [u8; wrap::PUBLIC_KEY_LEN] = b64decode(&public_key_b64)?
        .try_into()
        .map_err(|_| Error::Crypto)?;

    // Open our own grant to recover the vault key, then reseal it to the member.
    let mut vault_key = vault::open_vault_key(keypair, &env.enc_vault_key)?;
    let grant = vault::grant_vault_key(&public_key, &vault_key)?;
    vault_key.zeroize();

    api.create_grant(&env.id, member_user_id, &b64encode(&grant))?;
    Ok(env.id)
}

/// Clone a shared environment onto this device: fetch our own grant, reconstruct the project + env
/// locally (with caller-supplied labels, since names aren't shared-readable yet), and pull its
/// secrets. `org_id` — the owning org — is recorded in the config so later pushes match server-side.
#[allow(clippy::too_many_arguments)]
pub fn clone_env(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &wrap::Keypair,
    project_id: &str,
    env_id: &str,
    project_label: &str,
    env_label: &str,
    org_id: Option<&str>,
) -> Result<Config> {
    let grant_b64 = api.get_grant(env_id)?.ok_or_else(|| {
        Error::Input("you have not been granted this environment (ask an admin to share it)".into())
    })?;
    let grant = b64decode(&grant_b64)?;
    // Prove the grant opens with our keypair before persisting anything (fail closed on a bad grant).
    vault::open_vault_key(keypair, &grant)?;

    if store.get_project(project_id)?.is_none() {
        store.create_project_with_id(project_id, project_label)?;
    }
    if store.find_environment(env_id)?.is_none() {
        store.create_environment(env_id, project_id, env_label, &grant)?;
    }

    let config = Config {
        project_id: project_id.to_string(),
        project: project_label.to_string(),
        environment: env_label.to_string(),
        org_id: org_id.map(str::to_string),
    };
    super::sync::pull(api, store, &config)?;
    Ok(config)
}
