//! The sync engine: pull-rebase-push reconciliation of one environment's secrets.
//!
//! Secrets are opaque ciphertext whose AAD binds the (matching) `env_id`, secret id, and version,
//! so reconciliation moves blobs verbatim - never re-encrypting. `pull` applies the server snapshot
//! to the local store (server wins on a newer version, or an equal-version server-side tombstone);
//! `push` fast-forwards from a fresh snapshot, diffs local-vs-server, writes the batch at that
//! `base_revision`, and retries on a concurrency conflict (412). Project/environment names are the
//! one thing encrypted here (under the master key) for the server's zero-knowledge `enc_name`.

use std::collections::HashMap;
use std::time::Duration;

use sotto_core::{kdf, names};
use zeroize::Zeroize;

use crate::account;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::session;
use crate::store::{AccountKeys, Environment, Project, Store, SyncSecret};

use super::api::{
    b64decode, b64encode, AccountBundle, BatchRequest, NewEnvironment, NewProject, SecretChange,
    Snapshot, SyncApi,
};

/// Bound on pull-rebase-push retries before giving up under sustained concurrent writes.
const MAX_SYNC_ATTEMPTS: usize = 5;

/// Pull the active environment: apply the server snapshot locally and return the current revision.
pub fn pull(api: &dyn SyncApi, store: &Store, config: &Config) -> Result<i64> {
    let env = resolve_env(store, config)?;
    let base = store.synced_revision(&env.id)?;
    match api.snapshot(&env.id, Some(base))? {
        None => Ok(base), // 304 Not Modified
        Some(snapshot) => {
            // An org env may have been rotated: if our grant changed, adopt the new vault key and
            // force-refresh the rewrapped data keys (they keep the old versions the gate would skip).
            let rotated =
                config.org_id.is_some() && adopt_rotation(api, store, &env.id, &env.enc_vault_key)?;
            apply_snapshot(store, &env.id, &snapshot, rotated)?;
            store.set_synced_revision(&env.id, snapshot.revision)?;
            Ok(snapshot.revision)
        }
    }
}

/// If our grant for `env_id` changed on the server (a key rotation), store the new grant locally and
/// return `true` so the caller force-refreshes the rewrapped data keys. `false` when the grant is
/// unchanged or absent.
fn adopt_rotation(
    api: &dyn SyncApi,
    store: &Store,
    env_id: &str,
    local_grant: &[u8],
) -> Result<bool> {
    let Some(grant_b64) = api.get_grant(env_id)? else {
        return Ok(false);
    };
    let grant = b64decode(&grant_b64)?;
    if grant == local_grant {
        return Ok(false);
    }
    store.update_env_vault_key(env_id, &grant)?;
    Ok(true)
}

/// Whether our grant for `env_id` differs from `local_grant` (without adopting it). A missing grant
/// (404) counts as changed: a rotation dropped us from the grant set, so our local key is stale and
/// pushing under it would upload secrets the team can no longer decrypt - fail closed.
fn grant_changed(api: &dyn SyncApi, env_id: &str, local_grant: &[u8]) -> Result<bool> {
    match api.get_grant(env_id)? {
        Some(grant_b64) => Ok(b64decode(&grant_b64)? != local_grant),
        None => Ok(true),
    }
}

/// Push the active environment: ensure account/project/environment exist server-side, then
/// reconcile and upload local changes. Returns the resulting revision.
pub fn push(api: &dyn SyncApi, store: &Store, master: &[u8; 32], config: &Config) -> Result<i64> {
    let env = resolve_env(store, config)?;
    let project = store
        .get_project(&config.project_id)?
        .ok_or_else(|| Error::NotFound(format!("project `{}`", config.project_id)))?;

    ensure_account(api, store)?;
    let key = name_key(api, store, master, config.org_id.as_deref());
    ensure_project_env(api, &key, config.org_id.as_deref(), &project, &env)?;

    for _ in 0..MAX_SYNC_ATTEMPTS {
        // Fast-forward from the latest snapshot, then diff against it.
        let snapshot = api
            .snapshot(&env.id, None)?
            .ok_or_else(|| Error::Server("server returned no snapshot".into()))?;
        apply_snapshot(store, &env.id, &snapshot, false)?;
        store.set_synced_revision(&env.id, snapshot.revision)?;

        let changes = diff(store, &env.id, &snapshot)?;
        if changes.is_empty() {
            // Nothing to push. Adopt a rotation if one happened, so our local key stays current.
            if config.org_id.is_some() && adopt_rotation(api, store, &env.id, &env.enc_vault_key)? {
                apply_snapshot(store, &env.id, &snapshot, true)?;
            }
            return Ok(snapshot.revision);
        }

        // We have local changes. If the env was rotated, they're encrypted under the old vault key -
        // refuse rather than upload data the team can no longer decrypt.
        if config.org_id.is_some() && grant_changed(api, &env.id, &env.enc_vault_key)? {
            return Err(Error::Conflict(
                "environment was rotated; run `sotto pull` and re-apply your changes".into(),
            ));
        }

        let batch = BatchRequest {
            base_revision: snapshot.revision,
            changes,
        };
        match api.write_secrets(&env.id, &batch) {
            Ok(resp) => {
                store.set_synced_revision(&env.id, resp.revision)?;
                return Ok(resp.revision);
            }
            // Someone else advanced the revision between our snapshot and write - re-pull and retry.
            Err(Error::Conflict(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::Conflict(
        "sync: too many concurrent updates; try again".into(),
    ))
}

fn resolve_env(store: &Store, config: &Config) -> Result<Environment> {
    store
        .get_environment(&config.project_id, &config.environment)?
        .ok_or_else(|| Error::NotFound(format!("environment `{}`", config.environment)))
}

/// Upload account crypto material on first push; a 409 means it's already initialised (fine).
fn ensure_account(api: &dyn SyncApi, store: &Store) -> Result<()> {
    let material = account::material(store)?;
    let bundle = AccountBundle {
        public_key: b64encode(&material.public_key),
        enc_private_keys: b64encode(&material.enc_private_keys),
        kdf_params: b64encode(&material.kdf_params),
        recovery_blob: b64encode(&material.recovery_blob),
    };
    match api.put_account(&bundle) {
        Ok(()) | Err(Error::Conflict(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The key display names are encrypted under: the org key for an org project (when this account
/// holds a copy), else the master key. Falling back to the master keeps pushes working for an org
/// the caller has no org key for - the names are then creator-readable only, as before org keys.
fn name_key(api: &dyn SyncApi, store: &Store, master: &[u8; 32], org_id: Option<&str>) -> [u8; 32] {
    if let Some(org) = org_id {
        if let Ok(keypair) = account_keypair(store, master) {
            if let Ok(Some(key)) = super::team::org_key(api, &keypair, org) {
                return key;
            }
        }
    }
    *master
}

/// Recover the account keypair from the local store's sealed private keys.
fn account_keypair(store: &Store, master: &[u8; 32]) -> Result<sotto_core::wrap::Keypair> {
    let keys = store.get_account_keys()?.ok_or(Error::NoIdentity)?;
    sotto_core::vault::open_account_keypair(master, &keys.enc_private_keys).map_err(Into::into)
}

/// Idempotently create the project + environment server-side (encrypting their names under
/// `key` - the org key for org projects, else the master key). `org_id`, when set, creates the
/// project under that organisation (the caller must be an admin+ of it).
///
/// On an org-owned project a plain member lacks the admin+ rights these structural creates require,
/// so the server answers 403 - but the member cloned an environment that already exists and may
/// still write its secrets, so that 403 is expected and non-fatal. Any other error, or a 403 on a
/// personal project, stays fatal. A member whose env genuinely doesn't exist server-side still fails
/// later, as a 404 from the snapshot call.
fn ensure_project_env(
    api: &dyn SyncApi,
    key: &[u8; 32],
    org_id: Option<&str>,
    project: &Project,
    env: &Environment,
) -> Result<()> {
    tolerate_org_forbidden(
        api.create_project(&NewProject {
            id: project.id.clone(),
            enc_name: b64encode(&names::encrypt_project_name(
                key,
                &project.id,
                project.name.as_bytes(),
            )),
            org_id: org_id.map(str::to_string),
        }),
        org_id,
    )?;
    tolerate_org_forbidden(
        api.create_environment(
            &project.id,
            &NewEnvironment {
                id: env.id.clone(),
                enc_name: b64encode(&names::encrypt_env_name(key, &env.id, env.name.as_bytes())),
                enc_vault_key: b64encode(&env.enc_vault_key),
            },
        ),
        org_id,
    )
}

/// Swallow a 403 from an idempotent structural create when the project is org-owned (see
/// [`ensure_project_env`]); pass every other outcome through unchanged.
fn tolerate_org_forbidden(res: Result<()>, org_id: Option<&str>) -> Result<()> {
    match res {
        Err(Error::Forbidden(_)) if org_id.is_some() => Ok(()),
        other => other,
    }
}

/// Apply a server snapshot to the local store: server wins on a strictly newer version, or on an
/// equal-version tombstone the server introduced. `force` overrides the version gate to overwrite
/// every secret - used after a key rotation, where the data keys were rewrapped in place (same
/// version) and would otherwise be missed.
fn apply_snapshot(store: &Store, env_id: &str, snapshot: &Snapshot, force: bool) -> Result<()> {
    for remote in &snapshot.secrets {
        let local = store.find_secret(env_id, &remote.id)?;
        let apply = force
            || match &local {
                None => true,
                Some(local) => {
                    remote.version > local.version
                        || (remote.version == local.version && remote.deleted && !local.deleted)
                }
            };
        if apply {
            store.put_remote_secret(
                env_id,
                &SyncSecret {
                    id: remote.id.clone(),
                    enc_name: b64decode(&remote.enc_name)?,
                    enc_value: b64decode(&remote.enc_value)?,
                    enc_data_key: b64decode(&remote.enc_data_key)?,
                    version: remote.version,
                    deleted: remote.deleted,
                },
            )?;
        }
    }
    Ok(())
}

/// Compute the changes to push: local secrets newer than the server, or locally-deleted ones the
/// server still has live. (The caller has already fast-forwarded local from this snapshot.)
fn diff(store: &Store, env_id: &str, snapshot: &Snapshot) -> Result<Vec<SecretChange>> {
    let server: HashMap<&str, &super::api::SecretEntry> = snapshot
        .secrets
        .iter()
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut changes = Vec::new();
    for local in store.all_secrets(env_id)? {
        match server.get(local.id.as_str()) {
            None => {
                if !local.deleted {
                    changes.push(set_change(local));
                }
            }
            Some(remote) => {
                if !local.deleted && local.version > remote.version {
                    changes.push(set_change(local));
                } else if local.deleted && !remote.deleted {
                    changes.push(SecretChange::delete(local.id));
                }
            }
        }
    }
    Ok(changes)
}

fn set_change(local: SyncSecret) -> SecretChange {
    SecretChange::set(
        local.id,
        local.version,
        b64encode(&local.enc_name),
        b64encode(&local.enc_value),
        b64encode(&local.enc_data_key),
    )
}

/// One decrypted history version of a secret.
pub struct HistoryVersion {
    pub version: i64,
    /// The decrypted value, or `None` when this row doesn't authenticate under the current vault
    /// key (it predates a rotation this device hasn't caught up with - `sotto pull` first).
    pub value: Option<Vec<u8>>,
}

/// The server-side version history of one secret, newest first, decrypted under the current vault
/// key. The server is the source of truth for history - local stores only hold the versions they
/// happened to sync - and rotation rewraps history keys, so every row should decrypt.
pub fn history(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &sotto_core::wrap::Keypair,
    config: &Config,
    name: &str,
) -> Result<Vec<HistoryVersion>> {
    let (vault, secret_id) = open_secret(store, keypair, config, name)?;
    let mut versions: Vec<HistoryVersion> = api
        .list_history(vault.env_id())?
        .into_iter()
        .filter(|row| row.secret_id == secret_id)
        .map(|row| {
            let value = vault
                .decrypt_at(
                    &secret_id,
                    row.version,
                    &b64decode(&row.enc_value)?,
                    &b64decode(&row.enc_data_key)?,
                )
                .ok();
            Ok(HistoryVersion {
                version: row.version,
                value,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if versions.is_empty() {
        return Err(Error::NotFound(format!(
            "no server history for `{name}`; has this environment been pushed?"
        )));
    }
    versions.sort_by_key(|v| std::cmp::Reverse(v.version));
    Ok(versions)
}

/// Roll a secret back: decrypt `version`'s value from the server history and set it as a NEW
/// version through the normal set path (monotonic versions, normal sync - nothing is rewritten).
/// The change is local until the next `push`. Returns the restored value's byte length.
pub fn rollback(
    api: &dyn SyncApi,
    store: &Store,
    keypair: &sotto_core::wrap::Keypair,
    config: &Config,
    name: &str,
    version: i64,
) -> Result<usize> {
    let (vault, secret_id) = open_secret(store, keypair, config, name)?;
    let row = api
        .list_history(vault.env_id())?
        .into_iter()
        .find(|row| row.secret_id == secret_id && row.version == version)
        .ok_or_else(|| Error::NotFound(format!("`{name}` has no version {version}")))?;
    let mut value = vault
        .decrypt_at(
            &secret_id,
            version,
            &b64decode(&row.enc_value)?,
            &b64decode(&row.enc_data_key)?,
        )
        .map_err(|_| {
            Error::Input(
                "that version doesn't decrypt under the current vault key; run `sotto pull` and try again"
                    .into(),
            )
        })?;
    let len = value.len();
    let result = vault.set(name, &value);
    value.zeroize();
    result?;
    Ok(len)
}

/// Open the active environment's vault and resolve `name` to its secret id (live or tombstoned).
fn open_secret<'a>(
    store: &'a Store,
    keypair: &sotto_core::wrap::Keypair,
    config: &Config,
    name: &str,
) -> Result<(crate::vault::Vault<'a>, String)> {
    let vault = crate::vault::Vault::open(store, keypair, &config.project_id, &config.environment)?;
    let secret_id = vault
        .find_id_by_name(name)?
        .ok_or_else(|| Error::NotFound(name.to_string()))?;
    Ok((vault, secret_id))
}

/// Reconstruct the local identity on a new device from a downloaded account bundle plus the pasted
/// secret key and master password. Decodes the bundle and delegates to [`session::restore`].
pub fn restore_account(
    store: &Store,
    keychain: &dyn Keychain,
    bundle: &AccountBundle,
    secret_key: &[u8],
    password: &[u8],
    ttl: Duration,
) -> Result<()> {
    let params = account::KdfParams::from_bytes(&b64decode(&bundle.kdf_params)?)?;
    let salt: [u8; kdf::SALT_LEN] = params
        .salt
        .as_slice()
        .try_into()
        .map_err(|_| Error::Crypto)?;
    let account_keys = AccountKeys {
        public_key: b64decode(&bundle.public_key)?,
        enc_private_keys: b64decode(&bundle.enc_private_keys)?,
        recovery_blob: b64decode(&bundle.recovery_blob)?,
    };
    session::restore(
        store,
        keychain,
        password,
        secret_key,
        &salt,
        &account_keys,
        ttl,
    )
}

/// Reconstruct the local project + environments from the server. Env names decrypt under the org
/// key (org projects) or the master key (personal); an undecryptable name falls back to the env
/// id. Environments the caller holds no grant for are skipped - they couldn't be opened anyway.
/// Existing local rows are left untouched. Run after [`restore_account`], before [`pull`], on a
/// new device.
pub fn pull_environments(
    api: &dyn SyncApi,
    store: &Store,
    master: &[u8; 32],
    config: &Config,
) -> Result<()> {
    if store.get_project(&config.project_id)?.is_none() {
        store.create_project_with_id(&config.project_id, &config.project)?;
    }
    let key = name_key(api, store, master, config.org_id.as_deref());
    for env in api.list_environments(&config.project_id)? {
        if store.find_environment(&env.id)?.is_some() {
            continue;
        }
        let Some(grant_b64) = &env.enc_vault_key else {
            continue; // no grant for this env: nothing we could ever decrypt
        };
        // Try the resolved key, then the master (an org env pushed before the org key existed),
        // then fall back to the id - a missing display name must not block reconstruction.
        let enc_name = b64decode(&env.enc_name)?;
        let name = names::decrypt_env_name(&key, &env.id, &enc_name)
            .or_else(|_| names::decrypt_env_name(master, &env.id, &enc_name))
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| env.id.clone());
        store.create_environment(&env.id, &config.project_id, &name, &b64decode(grant_b64)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Project;
    use crate::vault::Vault;
    use std::cell::RefCell;
    use std::collections::HashSet;

    const MASTER: [u8; 32] = [0x42; 32];

    /// A fixed account keypair for the vault-key grants, shared by the mock "devices" (they are the
    /// same user, so their grants open with the same keypair). Independent of the name-encryption
    /// `MASTER` above.
    fn test_keypair() -> sotto_core::wrap::Keypair {
        sotto_core::wrap::keypair_from_secret(&[0x42; 32])
    }

    /// Recover a device's real account keypair (used by the reconstruction tests, where the vault
    /// keypair must genuinely match across devices).
    fn device_keypair(store: &Store, master: &[u8; 32]) -> sotto_core::wrap::Keypair {
        let enc_private_keys = store.get_account_keys().unwrap().unwrap().enc_private_keys;
        sotto_core::vault::open_account_keypair(master, &enc_private_keys).unwrap()
    }

    // --- a faithful in-memory server mirroring the real endpoints' semantics ---

    struct ServerSecret {
        enc_name: Vec<u8>,
        enc_value: Vec<u8>,
        enc_data_key: Vec<u8>,
        version: i64,
        deleted: bool,
    }

    /// One retained history version: `(enc_name, enc_value, enc_data_key)`.
    type HistoryRec = (Vec<u8>, Vec<u8>, Vec<u8>);

    struct EnvState {
        project_id: String,
        enc_name: String,
        revision: i64,
        secrets: HashMap<String, ServerSecret>,
        /// `(secret_id, version)` → `(enc_name, enc_value, enc_data_key)` - the retained history.
        history: HashMap<(String, i64), HistoryRec>,
    }

    struct MemberRec {
        role: String,
        public_key: Option<Vec<u8>>,
        /// The org key sealed to this member (base64), as the membership row carries it.
        enc_org_key: Option<String>,
    }

    struct OrgState {
        enc_name: String,
        members: HashMap<String, MemberRec>,
    }

    struct MachineTokenRec {
        env_id: String,
        public_key: Vec<u8>,
        enc_vault_key: String,
        revoked: bool,
    }

    #[derive(Default)]
    struct MockState {
        account: Option<AccountBundle>,
        projects: HashSet<String>,
        envs: HashMap<String, EnvState>,
        shares: HashMap<String, super::super::api::NewShare>,
        orgs: HashMap<String, OrgState>,
        /// `(env_id, user_id)` → the member's vault-key grant (base64).
        grants: HashMap<(String, String), String>,
        /// email → `(user_id, public_key)`, the pool of "existing users" invites resolve against.
        users_by_email: HashMap<String, (String, Option<Vec<u8>>)>,
        /// token_id → machine token record.
        machine_tokens: HashMap<String, MachineTokenRec>,
        fail_writes: u32,
    }

    #[derive(Default)]
    struct MockApi {
        state: RefCell<MockState>,
        /// Which user the next calls act as (empty = the default "test-user"). Lets one test drive
        /// the server as two different members, the way distinct sessions would.
        current_user: RefCell<String>,
    }

    impl MockApi {
        fn stored_share(&self, token: &str) -> Option<super::super::api::NewShare> {
            self.state.borrow().shares.get(token).cloned()
        }
        fn fail_next_write(&self) {
            self.state.borrow_mut().fail_writes += 1;
        }
        fn has_account(&self) -> bool {
            self.state.borrow().account.is_some()
        }
        fn current_user(&self) -> String {
            let u = self.current_user.borrow();
            if u.is_empty() {
                "test-user".to_string()
            } else {
                u.clone()
            }
        }
        /// Act as `user_id` for subsequent calls (as a different session would).
        fn as_user(&self, user_id: &str) {
            *self.current_user.borrow_mut() = user_id.to_string();
        }
        /// Register an existing user that invites can resolve by email.
        fn register_user(&self, email: &str, user_id: &str, public_key: &[u8]) {
            self.state.borrow_mut().users_by_email.insert(
                email.to_string(),
                (user_id.to_string(), Some(public_key.to_vec())),
            );
        }
    }

    impl SyncApi for MockApi {
        fn me(&self) -> Result<super::super::api::Me> {
            Ok(super::super::api::Me {
                user_id: "test-user".into(),
            })
        }

        fn put_account(&self, bundle: &AccountBundle) -> Result<()> {
            let mut s = self.state.borrow_mut();
            if s.account.is_some() {
                return Err(Error::Conflict("account already initialised".into()));
            }
            s.account = Some(bundle.clone());
            Ok(())
        }

        fn reset_account(&self, bundle: &AccountBundle) -> Result<()> {
            let me = self.current_user();
            let mut s = self.state.borrow_mut();
            if s.account.is_none() {
                return Err(Error::NotFound("account is not initialised".into()));
            }
            s.account = Some(bundle.clone());
            // Mirror the server: the caller's now-dead grants are deleted with the reset, and
            // (since the real server reads `users` live) their listed public key is the new one.
            s.grants.retain(|(_, uid), _| *uid != me);
            let new_key = b64decode(&bundle.public_key)?;
            for (uid, key) in s.users_by_email.values_mut() {
                if *uid == me {
                    *key = Some(new_key.clone());
                }
            }
            for org in s.orgs.values_mut() {
                if let Some(rec) = org.members.get_mut(&me) {
                    rec.public_key = Some(new_key.clone());
                    // Mirror the server: the sealed org key was for the dead keypair - cleared.
                    rec.enc_org_key = None;
                }
            }
            Ok(())
        }

        fn get_account(&self) -> Result<Option<AccountBundle>> {
            Ok(self.state.borrow().account.clone())
        }

        fn create_project(&self, project: &NewProject) -> Result<()> {
            self.state.borrow_mut().projects.insert(project.id.clone());
            Ok(())
        }

        fn create_environment(&self, project_id: &str, env: &NewEnvironment) -> Result<()> {
            let me = self.current_user();
            let mut s = self.state.borrow_mut();
            if !s.envs.contains_key(&env.id) {
                s.envs.insert(
                    env.id.clone(),
                    EnvState {
                        project_id: project_id.to_string(),
                        enc_name: env.enc_name.clone(),
                        revision: 0,
                        secrets: HashMap::new(),
                        history: HashMap::new(),
                    },
                );
                // Mirror the server: env creation records the creator's own grant.
                s.grants
                    .insert((env.id.clone(), me), env.enc_vault_key.clone());
            }
            Ok(())
        }

        fn list_environments(
            &self,
            project_id: &str,
        ) -> Result<Vec<super::super::api::EnvironmentInfo>> {
            // Mirror the server: each row carries the CALLER's own grant (or none).
            let me = self.current_user();
            let s = self.state.borrow();
            let mut envs: Vec<_> = s
                .envs
                .iter()
                .filter(|(_, e)| e.project_id == project_id)
                .map(|(id, e)| super::super::api::EnvironmentInfo {
                    id: id.clone(),
                    enc_name: e.enc_name.clone(),
                    enc_vault_key: s.grants.get(&(id.clone(), me.clone())).cloned(),
                    revision: e.revision,
                })
                .collect();
            envs.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(envs)
        }

        fn snapshot(&self, env_id: &str, if_none_match: Option<i64>) -> Result<Option<Snapshot>> {
            let s = self.state.borrow();
            let env = s
                .envs
                .get(env_id)
                .ok_or_else(|| Error::NotFound("environment not found".into()))?;
            if if_none_match == Some(env.revision) {
                return Ok(None);
            }
            let mut secrets: Vec<_> = env
                .secrets
                .iter()
                .map(|(id, sec)| super::super::api::SecretEntry {
                    id: id.clone(),
                    enc_name: b64encode(&sec.enc_name),
                    enc_value: b64encode(&sec.enc_value),
                    enc_data_key: b64encode(&sec.enc_data_key),
                    version: sec.version,
                    deleted: sec.deleted,
                })
                .collect();
            secrets.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Some(Snapshot {
                revision: env.revision,
                secrets,
            }))
        }

        fn write_secrets(
            &self,
            env_id: &str,
            batch: &BatchRequest,
        ) -> Result<super::super::api::BatchResponse> {
            let mut s = self.state.borrow_mut();
            if s.fail_writes > 0 {
                s.fail_writes -= 1;
                return Err(Error::Conflict("injected conflict".into()));
            }
            let env = s
                .envs
                .get_mut(env_id)
                .ok_or_else(|| Error::NotFound("environment not found".into()))?;
            if batch.base_revision != env.revision {
                return Err(Error::Conflict("stale base_revision".into()));
            }
            for change in &batch.changes {
                match change.op.as_str() {
                    "set" => {
                        env.secrets.insert(
                            change.id.clone(),
                            ServerSecret {
                                enc_name: b64decode(change.enc_name.as_ref().unwrap()).unwrap(),
                                enc_value: b64decode(change.enc_value.as_ref().unwrap()).unwrap(),
                                enc_data_key: b64decode(change.enc_data_key.as_ref().unwrap())
                                    .unwrap(),
                                version: change.version,
                                deleted: false,
                            },
                        );
                        // Mirror the server: every set appends a history row.
                        env.history
                            .entry((change.id.clone(), change.version))
                            .or_insert((
                                b64decode(change.enc_name.as_ref().unwrap()).unwrap(),
                                b64decode(change.enc_value.as_ref().unwrap()).unwrap(),
                                b64decode(change.enc_data_key.as_ref().unwrap()).unwrap(),
                            ));
                    }
                    "delete" => {
                        if let Some(sec) = env.secrets.get_mut(&change.id) {
                            sec.deleted = true;
                        }
                    }
                    other => panic!("unexpected op {other}"),
                }
            }
            env.revision += 1;
            Ok(super::super::api::BatchResponse {
                revision: env.revision,
            })
        }

        fn create_share(
            &self,
            share: &super::super::api::NewShare,
        ) -> Result<super::super::api::CreatedShare> {
            let token = uuid::Uuid::new_v4().to_string();
            self.state
                .borrow_mut()
                .shares
                .insert(token.clone(), share.clone());
            Ok(super::super::api::CreatedShare {
                token,
                expires_at: None,
            })
        }

        fn create_org(&self, org: &super::super::api::NewOrg) -> Result<()> {
            let me = self.current_user();
            let mut s = self.state.borrow_mut();
            let public_key = s
                .users_by_email
                .values()
                .find(|(uid, _)| *uid == me)
                .and_then(|(_, pk)| pk.clone());
            let mut members = HashMap::new();
            members.insert(
                me,
                MemberRec {
                    role: "owner".into(),
                    public_key,
                    enc_org_key: Some(org.enc_org_key.clone()),
                },
            );
            s.orgs.insert(
                org.id.clone(),
                OrgState {
                    enc_name: org.enc_name.clone(),
                    members,
                },
            );
            Ok(())
        }

        fn list_orgs(&self) -> Result<Vec<super::super::api::OrgInfo>> {
            let me = self.current_user();
            let s = self.state.borrow();
            let mut orgs: Vec<_> = s
                .orgs
                .iter()
                .filter_map(|(id, o)| {
                    o.members.get(&me).map(|rec| super::super::api::OrgInfo {
                        id: id.clone(),
                        enc_name: o.enc_name.clone(),
                        role: rec.role.clone(),
                        enc_org_key: rec.enc_org_key.clone(),
                    })
                })
                .collect();
            orgs.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(orgs)
        }

        fn grant_org_key(&self, org_id: &str, user_id: &str, enc_org_key: &str) -> Result<()> {
            let mut s = self.state.borrow_mut();
            let org = s
                .orgs
                .get_mut(org_id)
                .ok_or_else(|| Error::NotFound("organisation not found".into()))?;
            let member = org
                .members
                .get_mut(user_id)
                .ok_or_else(|| Error::NotFound("member not found".into()))?;
            member.enc_org_key = Some(enc_org_key.to_string());
            Ok(())
        }

        fn invite_member(&self, org_id: &str, email: &str) -> Result<super::super::api::Invited> {
            let mut s = self.state.borrow_mut();
            let (user_id, public_key) = s
                .users_by_email
                .get(email)
                .cloned()
                .ok_or_else(|| Error::NotFound("no user with that email".into()))?;
            let org = s
                .orgs
                .get_mut(org_id)
                .ok_or_else(|| Error::NotFound("organisation not found".into()))?;
            org.members.entry(user_id.clone()).or_insert(MemberRec {
                role: "member".into(),
                public_key: public_key.clone(),
                enc_org_key: None,
            });
            Ok(super::super::api::Invited {
                user_id,
                public_key: public_key.map(|k| b64encode(&k)),
            })
        }

        fn list_members(&self, org_id: &str) -> Result<Vec<super::super::api::MemberInfo>> {
            let s = self.state.borrow();
            let org = s
                .orgs
                .get(org_id)
                .ok_or_else(|| Error::NotFound("organisation not found".into()))?;
            let mut members: Vec<_> = org
                .members
                .iter()
                .map(|(uid, rec)| super::super::api::MemberInfo {
                    user_id: uid.clone(),
                    role: rec.role.clone(),
                    public_key: rec.public_key.as_ref().map(|k| b64encode(k)),
                })
                .collect();
            members.sort_by(|a, b| a.user_id.cmp(&b.user_id));
            Ok(members)
        }

        fn create_grant(&self, env_id: &str, user_id: &str, enc_vault_key: &str) -> Result<()> {
            self.state.borrow_mut().grants.insert(
                (env_id.to_string(), user_id.to_string()),
                enc_vault_key.to_string(),
            );
            Ok(())
        }

        fn get_grant(&self, env_id: &str) -> Result<Option<String>> {
            let me = self.current_user();
            Ok(self
                .state
                .borrow()
                .grants
                .get(&(env_id.to_string(), me))
                .cloned())
        }

        fn list_grant_holders(&self, env_id: &str) -> Result<Vec<String>> {
            let s = self.state.borrow();
            let mut holders: Vec<String> = s
                .grants
                .keys()
                .filter(|(e, _)| e == env_id)
                .map(|(_, u)| u.clone())
                .collect();
            holders.sort();
            Ok(holders)
        }

        fn member_env_grants(&self, _org_id: &str, user_id: &str) -> Result<Vec<String>> {
            let s = self.state.borrow();
            let mut envs: Vec<String> = s
                .grants
                .keys()
                .filter(|(_, u)| u == user_id)
                .map(|(e, _)| e.clone())
                .collect();
            envs.sort();
            envs.dedup();
            Ok(envs)
        }

        fn org_entitlements(&self, _org_id: &str) -> Result<super::super::api::Entitlements> {
            // Like the audit log, plans are a server-side concern with no client crypto; the
            // server DB tests own the behaviour.
            Ok(super::super::api::Entitlements {
                tier: "free".into(),
                effective_tier: "team".into(),
                trial_ends_at: None,
                limits: None,
            })
        }

        fn org_audit(
            &self,
            _org_id: &str,
            _limit: Option<i64>,
        ) -> Result<Vec<super::super::api::AuditEvent>> {
            // Audit is a server-side ledger with no client crypto; its behaviour is covered by the
            // server DB tests, so the mock has nothing to prove and returns an empty log.
            Ok(Vec::new())
        }

        fn list_history(&self, env_id: &str) -> Result<Vec<super::super::api::HistoryRow>> {
            let s = self.state.borrow();
            let env = s
                .envs
                .get(env_id)
                .ok_or_else(|| Error::NotFound("environment not found".into()))?;
            let mut rows: Vec<_> = env
                .history
                .iter()
                .map(
                    |((secret_id, version), (enc_name, enc_value, enc_data_key))| {
                        super::super::api::HistoryRow {
                            secret_id: secret_id.clone(),
                            version: *version,
                            enc_name: b64encode(enc_name),
                            enc_value: b64encode(enc_value),
                            enc_data_key: b64encode(enc_data_key),
                        }
                    },
                )
                .collect();
            rows.sort_by(|a, b| (&a.secret_id, a.version).cmp(&(&b.secret_id, b.version)));
            Ok(rows)
        }

        fn rotate(
            &self,
            env_id: &str,
            req: &super::super::api::RotateRequest,
        ) -> Result<super::super::api::RotateResponse> {
            let mut s = self.state.borrow_mut();
            let new_rev = {
                let env = s
                    .envs
                    .get_mut(env_id)
                    .ok_or_else(|| Error::NotFound("environment not found".into()))?;
                if req.base_revision != env.revision {
                    return Err(Error::Conflict("stale base_revision".into()));
                }
                for dk in &req.data_keys {
                    if let Some(sec) = env.secrets.get_mut(&dk.secret_id) {
                        sec.enc_data_key = b64decode(&dk.enc_data_key)?;
                    }
                }
                // History coverage must be exact (as the server enforces), then rows are rewrapped.
                let existing: HashSet<(String, i64)> = env.history.keys().cloned().collect();
                let provided: HashSet<(String, i64)> = req
                    .history_keys
                    .iter()
                    .map(|h| (h.secret_id.clone(), h.version))
                    .collect();
                if provided != existing {
                    return Err(Error::Server(
                        "rotation must rewrap exactly the environment's retained history".into(),
                    ));
                }
                for h in &req.history_keys {
                    if let Some(row) = env.history.get_mut(&(h.secret_id.clone(), h.version)) {
                        row.2 = b64decode(&h.enc_data_key)?;
                    }
                }
                env.revision += 1;
                env.revision
            };
            // Replace the env's grant set wholesale, as the server does.
            s.grants.retain(|(e, _), _| e != env_id);
            for g in &req.grants {
                s.grants.insert(
                    (env_id.to_string(), g.user_id.clone()),
                    g.enc_vault_key.clone(),
                );
            }
            // Machine grants must cover exactly the env's active tokens (as the server enforces),
            // and each covered token's stored grant is re-sealed.
            let active: HashSet<String> = s
                .machine_tokens
                .iter()
                .filter(|(_, t)| t.env_id == env_id && !t.revoked)
                .map(|(id, _)| id.clone())
                .collect();
            let provided: HashSet<String> = req
                .machine_grants
                .iter()
                .map(|m| m.token_id.clone())
                .collect();
            if provided != active {
                return Err(Error::Server(
                    "rotation must re-grant exactly the environment's active machine tokens".into(),
                ));
            }
            for m in &req.machine_grants {
                if let Some(t) = s.machine_tokens.get_mut(&m.token_id) {
                    t.enc_vault_key = m.enc_vault_key.clone();
                }
            }
            Ok(super::super::api::RotateResponse { revision: new_rev })
        }

        fn remove_member(&self, org_id: &str, user_id: &str) -> Result<()> {
            let mut s = self.state.borrow_mut();
            if let Some(org) = s.orgs.get_mut(org_id) {
                org.members.remove(user_id);
            }
            Ok(())
        }

        fn create_machine_token(
            &self,
            env_id: &str,
            _name: &str,
            public_key: &str,
            enc_vault_key: &str,
        ) -> Result<super::super::api::CreatedMachineToken> {
            let token_id = uuid::Uuid::new_v4().to_string();
            self.state.borrow_mut().machine_tokens.insert(
                token_id.clone(),
                MachineTokenRec {
                    env_id: env_id.to_string(),
                    public_key: b64decode(public_key)?,
                    enc_vault_key: enc_vault_key.to_string(),
                    revoked: false,
                },
            );
            Ok(super::super::api::CreatedMachineToken {
                token_id,
                token: format!("smt_mock_{}", uuid::Uuid::new_v4()),
            })
        }

        fn list_machine_tokens(
            &self,
            env_id: &str,
        ) -> Result<Vec<super::super::api::MachineTokenInfo>> {
            let s = self.state.borrow();
            let mut tokens: Vec<_> = s
                .machine_tokens
                .iter()
                .filter(|(_, t)| t.env_id == env_id && !t.revoked)
                .map(|(id, t)| super::super::api::MachineTokenInfo {
                    token_id: id.clone(),
                    name: "ci".into(),
                    public_key: b64encode(&t.public_key),
                })
                .collect();
            tokens.sort_by(|a, b| a.token_id.cmp(&b.token_id));
            Ok(tokens)
        }

        fn revoke_machine_token(&self, _env_id: &str, token_id: &str) -> Result<()> {
            let mut s = self.state.borrow_mut();
            let t = s
                .machine_tokens
                .get_mut(token_id)
                .ok_or_else(|| Error::NotFound("machine token not found".into()))?;
            t.revoked = true;
            Ok(())
        }
    }

    /// A store with an initialised account + a project (dev/staging/prod) and its config.
    fn device() -> (Store, Project, Config) {
        let store = Store::open_in_memory().unwrap();
        let kc = crate::keychain::MemoryKeychain::default();
        crate::session::init(&store, &kc, b"pw", std::time::Duration::from_secs(3600)).unwrap();
        let project = Vault::create_project(&store, &test_keypair(), "acme").unwrap();
        let config = Config {
            project_id: project.id.clone(),
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        (store, project, config)
    }

    /// A second device mirroring `src`'s project + dev environment (same ids + wrapped vault key),
    /// as a real new device would after environment sync.
    fn mirror(src: &Store, project: &Project) -> Store {
        let store = Store::open_in_memory().unwrap();
        // A real second device has its own initialised identity/account material (its push's
        // duplicate account upload is ignored by the server). The vault still uses MASTER directly.
        let kc = crate::keychain::MemoryKeychain::default();
        crate::session::init(&store, &kc, b"pw", std::time::Duration::from_secs(3600)).unwrap();
        store
            .create_project_with_id(&project.id, &project.name)
            .unwrap();
        let dev = src.get_environment(&project.id, "dev").unwrap().unwrap();
        store
            .create_environment(&dev.id, &project.id, "dev", &dev.enc_vault_key)
            .unwrap();
        store
    }

    #[test]
    fn push_provisions_and_round_trips_through_pull() {
        let api = MockApi::default();
        let (store, project, config) = device();
        Vault::open(&store, &test_keypair(), &project.id, "dev")
            .unwrap()
            .set("DATABASE_URL", b"postgres://prod")
            .unwrap();

        let rev = push(&api, &store, &MASTER, &config).unwrap();
        assert_eq!(rev, 1);
        assert!(api.has_account());

        // A second device pulls and decrypts the same value.
        let b = mirror(&store, &project);
        assert_eq!(pull(&api, &b, &config).unwrap(), 1);
        let value = Vault::open(&b, &test_keypair(), &project.id, "dev")
            .unwrap()
            .get("DATABASE_URL")
            .unwrap();
        assert_eq!(value, b"postgres://prod");
    }

    #[test]
    fn updates_and_deletes_propagate() {
        let api = MockApi::default();
        let (store, project, config) = device();
        let a = Vault::open(&store, &test_keypair(), &project.id, "dev").unwrap();
        a.set("KEY", b"v1").unwrap();
        push(&api, &store, &MASTER, &config).unwrap();
        let b = mirror(&store, &project);
        pull(&api, &b, &config).unwrap();

        // Update on A → pull on B.
        a.set("KEY", b"v2").unwrap();
        push(&api, &store, &MASTER, &config).unwrap();
        pull(&api, &b, &config).unwrap();
        assert_eq!(
            Vault::open(&b, &test_keypair(), &project.id, "dev")
                .unwrap()
                .get("KEY")
                .unwrap(),
            b"v2"
        );

        // Delete on A → tombstone reaches B.
        a.delete("KEY").unwrap();
        push(&api, &store, &MASTER, &config).unwrap();
        pull(&api, &b, &config).unwrap();
        assert!(matches!(
            Vault::open(&b, &test_keypair(), &project.id, "dev")
                .unwrap()
                .get("KEY"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn pull_when_unchanged_is_a_noop() {
        let api = MockApi::default();
        let (store, project, config) = device();
        Vault::open(&store, &test_keypair(), &project.id, "dev")
            .unwrap()
            .set("K", b"v")
            .unwrap();
        let rev = push(&api, &store, &MASTER, &config).unwrap();
        // Pulling again returns the same revision (server responds 304) and changes nothing.
        assert_eq!(pull(&api, &store, &config).unwrap(), rev);
    }

    #[test]
    fn concurrent_writers_converge() {
        let api = MockApi::default();
        let (store, project, config) = device();
        let b = {
            // B must exist on the server first; A's push provisions the env.
            Vault::open(&store, &test_keypair(), &project.id, "dev")
                .unwrap()
                .set("AKEY", b"a0")
                .unwrap();
            push(&api, &store, &MASTER, &config).unwrap();
            mirror(&store, &project)
        };
        pull(&api, &b, &config).unwrap();

        // B writes BKEY and pushes; then A writes another key and pushes (its internal pull
        // rebases onto B's revision).
        Vault::open(&b, &test_keypair(), &project.id, "dev")
            .unwrap()
            .set("BKEY", b"b0")
            .unwrap();
        push(&api, &b, &MASTER, &config).unwrap();

        Vault::open(&store, &test_keypair(), &project.id, "dev")
            .unwrap()
            .set("CKEY", b"c0")
            .unwrap();
        push(&api, &store, &MASTER, &config).unwrap();
        pull(&api, &store, &config).unwrap();

        let a = Vault::open(&store, &test_keypair(), &project.id, "dev").unwrap();
        assert_eq!(a.get("AKEY").unwrap(), b"a0");
        assert_eq!(a.get("BKEY").unwrap(), b"b0");
        assert_eq!(a.get("CKEY").unwrap(), b"c0");
    }

    #[test]
    fn push_retries_after_a_conflict() {
        let api = MockApi::default();
        let (store, project, config) = device();
        Vault::open(&store, &test_keypair(), &project.id, "dev")
            .unwrap()
            .set("K", b"v")
            .unwrap();
        api.fail_next_write(); // first write_secrets returns 412
                               // The engine re-pulls and retries, succeeding on the second attempt.
        assert_eq!(push(&api, &store, &MASTER, &config).unwrap(), 1);
    }

    /// A real device whose vault uses the *derived* master key (not the `MASTER` constant), so the
    /// account material is consistent with the secrets - required to test reconstruction.
    fn real_device() -> (
        Store,
        [u8; 32],
        crate::session::EmergencyKit,
        Project,
        Config,
    ) {
        let store = Store::open_in_memory().unwrap();
        let kc = crate::keychain::MemoryKeychain::default();
        let kit =
            crate::session::init(&store, &kc, b"pw", std::time::Duration::from_secs(3600)).unwrap();
        let master = *crate::session::current_master_key(&kc)
            .unwrap()
            .unwrap()
            .as_bytes();
        let keypair = device_keypair(&store, &master);
        let project = Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = Config {
            project_id: project.id.clone(),
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        (store, master, kit, project, config)
    }

    #[test]
    fn new_device_reconstructs_identity_environment_and_secrets() {
        let api = MockApi::default();
        let (store_a, master_a, kit, project, config) = real_device();
        Vault::open(
            &store_a,
            &device_keypair(&store_a, &master_a),
            &project.id,
            "dev",
        )
        .unwrap()
        .set("DATABASE_URL", b"postgres://prod")
        .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();

        // New device: fresh store, reconstruct identity from the server account + Emergency Kit.
        let store_b = Store::open_in_memory().unwrap();
        let kc_b = crate::keychain::MemoryKeychain::default();
        let bundle = api.get_account().unwrap().unwrap();
        let secret_key = sotto_core::format::decode_key("SK", 1, &kit.secret_key).unwrap();
        restore_account(
            &store_b,
            &kc_b,
            &bundle,
            &secret_key,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master_b = *crate::session::current_master_key(&kc_b)
            .unwrap()
            .unwrap()
            .as_bytes();
        assert_eq!(
            master_a, master_b,
            "derived master key matches the original"
        );

        // Reconstruct environments (decrypting their names) and pull secrets.
        pull_environments(&api, &store_b, &master_b, &config).unwrap();
        pull(&api, &store_b, &config).unwrap();

        // The env name was decrypted, and the secret decrypts with the reconstructed master key.
        assert_eq!(store_b.list_environments(&project.id).unwrap(), vec!["dev"]);
        let value = Vault::open(
            &store_b,
            &device_keypair(&store_b, &master_b),
            &project.id,
            "dev",
        )
        .unwrap()
        .get("DATABASE_URL")
        .unwrap();
        assert_eq!(value, b"postgres://prod");
    }

    #[test]
    fn share_and_clone_gives_a_member_the_secrets() {
        use crate::remote::team;
        let api = MockApi::default();

        // Bob is an existing user with an account keypair; register his public key for invites.
        let bob = sotto_core::wrap::generate_keypair();
        api.register_user("bob@example.test", "bob-user", &bob.public);

        // Alice: a real device whose vault keypair matches its account material.
        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);

        // Alice creates an org, invites Bob, and marks her project org-owned.
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        assert_eq!(
            team::invite(&api, &alice, &org_id, "bob@example.test")
                .unwrap()
                .user_id,
            "bob-user"
        );
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };

        // Alice writes a secret and pushes, then shares the dev environment with Bob.
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("API_KEY", b"s3cr3t")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();
        let env_id = team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();

        // Bob (a fresh device, acting as himself) clones the shared env and decrypts the secret.
        api.as_user("bob-user");
        let store_b = Store::open_in_memory().unwrap();
        let bob_config = team::clone_env(
            &api,
            &store_b,
            &bob,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id),
        )
        .unwrap();
        assert_eq!(bob_config.org_id.as_deref(), Some(org_id.as_str()));
        assert_eq!(
            Vault::open(&store_b, &bob, &project.id, "dev")
                .unwrap()
                .get("API_KEY")
                .unwrap(),
            b"s3cr3t"
        );

        // A different user, never granted this env, cannot clone it.
        api.as_user("carol-user");
        let store_c = Store::open_in_memory().unwrap();
        let carol = sotto_core::wrap::generate_keypair();
        assert!(team::clone_env(
            &api,
            &store_c,
            &carol,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id)
        )
        .is_err());
    }

    #[test]
    fn removing_a_member_rotates_the_env_and_locks_out_their_cached_key() {
        use crate::remote::team;
        let api = MockApi::default();

        // Alice (a real device) and Bob (a teammate) both have registered public keys.
        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);
        let bob = sotto_core::wrap::generate_keypair();
        api.register_user("alice@example.test", "test-user", &alice.public);
        api.register_user("bob@example.test", "bob-user", &bob.public);

        // Alice sets up the org + shared env and gives Bob a grant; Bob clones and reads.
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        team::invite(&api, &alice, &org_id, "bob@example.test").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("API_KEY", b"s3cr3t")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();
        let env_id = team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();

        api.as_user("bob-user");
        let store_b = Store::open_in_memory().unwrap();
        let bob_config = team::clone_env(
            &api,
            &store_b,
            &bob,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id),
        )
        .unwrap();
        assert_eq!(
            Vault::open(&store_b, &bob, &project.id, "dev")
                .unwrap()
                .get("API_KEY")
                .unwrap(),
            b"s3cr3t"
        );

        // Alice removes Bob (rotating the env), adopts the new key via pull, and writes a new secret.
        api.as_user("test-user");
        let report = team::remove_member(&api, &alice, &org_id, "bob-user").unwrap();
        assert_eq!(report.rotated, vec![env_id.clone()]);
        pull(&api, &store_a, &config).unwrap();
        assert_eq!(
            Vault::open(&store_a, &alice, &project.id, "dev")
                .unwrap()
                .get("API_KEY")
                .unwrap(),
            b"s3cr3t",
            "a remaining member still reads the rewrapped secret"
        );
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("POST_ROTATION", b"new-write")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();

        // Bob's grant is gone, and his cached old vault key can't read the post-rotation write.
        api.as_user("bob-user");
        assert!(api.get_grant(&env_id).unwrap().is_none());
        pull(&api, &store_b, &bob_config).unwrap();
        let bob_vault = Vault::open(&store_b, &bob, &project.id, "dev").unwrap();
        assert_eq!(bob_vault.get("API_KEY").unwrap(), b"s3cr3t"); // what he already had
        assert!(
            bob_vault.get("POST_ROTATION").is_err(),
            "the removed member's old key must not decrypt writes made after rotation"
        );
    }

    #[test]
    fn a_member_dropped_from_the_grant_set_cannot_push() {
        use crate::remote::team;
        let api = MockApi::default();

        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);

        // Bob is his own initialised device; his account keypair is what grants seal to.
        let store_b = Store::open_in_memory().unwrap();
        let kc_b = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store_b,
            &kc_b,
            b"pwB",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master_b = *crate::session::current_master_key(&kc_b)
            .unwrap()
            .unwrap()
            .as_bytes();
        let bob = device_keypair(&store_b, &master_b);

        // Both public keys are on file so the removal's rotation can re-grant the remaining member.
        api.register_user("alice@example.test", "test-user", &alice.public);
        api.register_user("bob@example.test", "bob-user", &bob.public);

        // Alice sets up the shared env, grants Bob, Bob clones it.
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        team::invite(&api, &alice, &org_id, "bob@example.test").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("API_KEY", b"s3cr3t")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();
        let env_id = team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();

        api.as_user("bob-user");
        let bob_config = team::clone_env(
            &api,
            &store_b,
            &bob,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id),
        )
        .unwrap();

        // Bob makes a local edit he hasn't pushed yet.
        Vault::open(&store_b, &bob, &project.id, "dev")
            .unwrap()
            .set("BOB_LOCAL", b"pending")
            .unwrap();

        // Alice rotates Bob out of the grant set (Bob never adopts the new key).
        api.as_user("test-user");
        team::remove_member(&api, &alice, &org_id, "bob-user").unwrap();

        // Regression: Bob's grant is gone (`get_grant` → None), so pushing his pending change must
        // fail closed rather than upload a secret under a vault key the rotated team can't decrypt.
        api.as_user("bob-user");
        assert!(api.get_grant(&env_id).unwrap().is_none());
        assert!(
            matches!(
                push(&api, &store_b, &master_b, &bob_config),
                Err(Error::Conflict(_))
            ),
            "a member dropped from the grant set must not be able to push"
        );
    }

    #[test]
    fn machine_token_survives_rotation() {
        use crate::remote::{machine, team};
        let api = MockApi::default();

        // Alice: an org env with one secret, pushed.
        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);
        api.register_user("alice@example.test", "test-user", &alice.public);
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("API_KEY", b"s3cr3t")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();
        let env = store_a
            .get_environment(&project.id, "dev")
            .unwrap()
            .unwrap();

        // Create a machine token; its grant opens to the same vault key the env uses.
        let token_str = team::create_machine_token(&api, &store_a, &alice, &config, "ci").unwrap();
        let machine_token = machine::parse_token(&token_str).unwrap();
        let read_machine_grant = |api: &MockApi| -> Vec<u8> {
            let s = api.state.borrow();
            let rec = s.machine_tokens.values().next().unwrap();
            b64decode(&rec.enc_vault_key).unwrap()
        };
        let vault_key_before =
            sotto_core::vault::open_vault_key(&machine_token.keypair, &read_machine_grant(&api))
                .unwrap();
        assert_eq!(
            vault_key_before,
            sotto_core::vault::open_vault_key(&alice, &env.enc_vault_key).unwrap()
        );

        // Rotate the env: the machine's grant is re-sealed to the NEW vault key automatically.
        team::rotate_env(&api, &alice, &org_id, &env.id, None)
            .unwrap()
            .unwrap();
        let vault_key_after =
            sotto_core::vault::open_vault_key(&machine_token.keypair, &read_machine_grant(&api))
                .unwrap();
        assert_ne!(
            vault_key_before, vault_key_after,
            "rotation changed the vault key"
        );

        // The rewrapped data key decrypts under the machine's post-rotation vault key.
        let snap = api.snapshot(&env.id, None).unwrap().unwrap();
        let secret = &snap.secrets[0];
        let value = sotto_core::vault::decrypt_value(
            &vault_key_after,
            &env.id,
            &secret.id,
            secret.version,
            &b64decode(&secret.enc_value).unwrap(),
            &b64decode(&secret.enc_data_key).unwrap(),
        )
        .unwrap();
        assert_eq!(value, b"s3cr3t");

        // A revoked token drops out of rotation coverage: revoke, rotate again - still succeeds.
        let token_id = api
            .state
            .borrow()
            .machine_tokens
            .keys()
            .next()
            .unwrap()
            .clone();
        api.revoke_machine_token(&env.id, &token_id).unwrap();
        team::rotate_env(&api, &alice, &org_id, &env.id, None)
            .unwrap()
            .unwrap();
    }

    #[test]
    fn account_reset_kills_old_grants_and_regrant_recovers_access() {
        use crate::remote::team;
        let api = MockApi::default();

        // Alice shares an org env with Bob; Bob reads it (the established happy path).
        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);
        let bob_old = sotto_core::wrap::generate_keypair();
        api.register_user("bob@example.test", "bob-user", &bob_old.public);
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        team::invite(&api, &alice, &org_id, "bob@example.test").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("API_KEY", b"s3cr3t")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();
        let env_id = team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();

        // Bob loses everything and resets: fresh keypair, new account material uploaded.
        api.as_user("bob-user");
        let bob_new = sotto_core::wrap::generate_keypair();
        api.reset_account(&AccountBundle {
            public_key: b64encode(&bob_new.public),
            enc_private_keys: b64encode(b"new-sealed-privkeys"),
            kdf_params: b64encode(b"new-kdf"),
            recovery_blob: b64encode(b"new-recovery"),
        })
        .unwrap();

        // His old grant died with the reset: cloning fails clean ("not granted"), not with a
        // confusing crypto error.
        assert!(api.get_grant(&env_id).unwrap().is_none());
        let store_b = Store::open_in_memory().unwrap();
        assert!(team::clone_env(
            &api,
            &store_b,
            &bob_new,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id)
        )
        .is_err());

        // Alice re-grants (sealing to Bob's NEW key, as listed fresh by the server) and Bob is back.
        api.as_user("test-user");
        team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();
        api.as_user("bob-user");
        team::clone_env(
            &api,
            &store_b,
            &bob_new,
            &project.id,
            &env_id,
            Some("acme"),
            Some("dev"),
            Some(&org_id),
        )
        .unwrap();
        assert_eq!(
            Vault::open(&store_b, &bob_new, &project.id, "dev")
                .unwrap()
                .get("API_KEY")
                .unwrap(),
            b"s3cr3t"
        );
        // The old keypair stays locked out: it can't open the new grant.
        let new_grant = b64decode(&api.get_grant(&env_id).unwrap().unwrap()).unwrap();
        assert!(sotto_core::vault::open_vault_key(&bob_old, &new_grant).is_err());
    }

    #[test]
    fn history_and_rollback_survive_rotation() {
        use crate::remote::team;
        let api = MockApi::default();

        // A real org env with three versions of one secret, pushed.
        let (store, master, _kit, project, config0) = real_device();
        let alice = device_keypair(&store, &master);
        api.register_user("alice@example.test", "test-user", &alice.public);
        let org_id = team::create_org(&api, &alice, "acme").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        // Server history records *synced* versions: push after each set so all three land.
        let vault = || Vault::open(&store, &alice, &project.id, "dev").unwrap();
        for value in [&b"v1"[..], b"v2", b"v3"] {
            vault().set("KEY", value).unwrap();
            push(&api, &store, &master, &config).unwrap();
        }

        // History lists all three versions, newest first, decrypted.
        let versions = history(&api, &store, &alice, &config, "KEY").unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|v| (v.version, v.value.clone().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                (3, b"v3".to_vec()),
                (2, b"v2".to_vec()),
                (1, b"v1".to_vec())
            ]
        );

        // Rotate, adopt the new key, and history STILL decrypts - the rotation rewrapped it.
        let env = store.get_environment(&project.id, "dev").unwrap().unwrap();
        team::rotate_env(&api, &alice, &org_id, &env.id, None)
            .unwrap()
            .unwrap();
        pull(&api, &store, &config).unwrap();
        let versions = history(&api, &store, &alice, &config, "KEY").unwrap();
        assert!(
            versions.iter().all(|v| v.value.is_some()),
            "all history versions decrypt under the post-rotation key"
        );

        // Rollback to v1: the old value lands as a NEW version (4), nothing rewritten.
        let len = rollback(&api, &store, &alice, &config, "KEY", 1).unwrap();
        assert_eq!(len, 2);
        assert_eq!(vault().get("KEY").unwrap(), b"v1");
        push(&api, &store, &master, &config).unwrap();
        let versions = history(&api, &store, &alice, &config, "KEY").unwrap();
        assert_eq!(versions[0].version, 4);
        assert_eq!(versions[0].value.as_deref(), Some(&b"v1"[..]));
        assert_eq!(versions.len(), 4, "history is append-only");

        // Rolling back to a nonexistent version fails cleanly.
        assert!(matches!(
            rollback(&api, &store, &alice, &config, "KEY", 99),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn org_key_gives_members_readable_names() {
        use crate::remote::team;
        let api = MockApi::default();

        // Alice: an org project pushed, names encrypted under the org key.
        let (store_a, master_a, _kit, project, config0) = real_device();
        let alice = device_keypair(&store_a, &master_a);
        let bob = sotto_core::wrap::generate_keypair();
        api.register_user("bob@example.test", "bob-user", &bob.public);
        let org_id = team::create_org(&api, &alice, "acme-team").unwrap();
        let config = Config {
            org_id: Some(org_id.clone()),
            ..config0
        };
        Vault::open(&store_a, &alice, &project.id, "dev")
            .unwrap()
            .set("K", b"v")
            .unwrap();
        push(&api, &store_a, &master_a, &config).unwrap();

        // The creator reads the org name back through their sealed org key.
        assert_eq!(team::list_orgs(&api, &alice).unwrap()[0].name, "acme-team");

        // Invite grants Bob the org key: he reads the org name with NO key shared out of band.
        team::invite(&api, &alice, &org_id, "bob@example.test").unwrap();
        api.as_user("bob-user");
        assert_eq!(team::list_orgs(&api, &bob).unwrap()[0].name, "acme-team");

        // Share + clone: the cloned env auto-labels with its REAL name - no `--as` needed.
        api.as_user("test-user");
        let env_id = team::share_env(&api, &store_a, &alice, &org_id, "bob-user", &config).unwrap();
        api.as_user("bob-user");
        let store_b = Store::open_in_memory().unwrap();
        let bob_config = team::clone_env(
            &api,
            &store_b,
            &bob,
            &project.id,
            &env_id,
            None,
            None,
            Some(&org_id),
        )
        .unwrap();
        assert_eq!(
            bob_config.environment, "dev",
            "env label decrypted via the org key"
        );
        assert_eq!(
            Vault::open(&store_b, &bob, &project.id, "dev")
                .unwrap()
                .get("K")
                .unwrap(),
            b"v"
        );

        // An account reset clears Bob's org-key copy: names fall back to the org id.
        api.reset_account(&AccountBundle {
            public_key: b64encode(&sotto_core::wrap::generate_keypair().public),
            enc_private_keys: b64encode(b"new-priv"),
            kdf_params: b64encode(b"new-kdf"),
            recovery_blob: b64encode(b"new-rec"),
        })
        .unwrap();
        assert_eq!(team::list_orgs(&api, &bob).unwrap()[0].name, org_id);
    }

    // Uses the full MockApi (which stores uploaded share blobs) to exercise share creation.
    #[test]
    fn share_create_seals_uploads_and_links() {
        use crate::remote::api::b64decode;
        use crate::remote::share::{create, ShareOptions};
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let api = MockApi::default();

        // No passphrase: the fragment key is the AEAD key.
        let opts = ShareOptions {
            max_views: 1,
            ttl_seconds: Some(3600),
            passphrase: None,
        };
        let link = create(&api, "https://app.sotto.dev/", b"api-token", &opts).unwrap();
        assert!(link.starts_with("https://app.sotto.dev/s/"));

        let (base, fragment) = link.split_once('#').unwrap();
        let token = base.rsplit('/').next().unwrap();
        let key: [u8; 32] = URL_SAFE_NO_PAD
            .decode(fragment)
            .unwrap()
            .try_into()
            .unwrap();
        let share = api.stored_share(token).unwrap();
        assert!(share.passphrase_salt.is_none());
        let enc_blob = b64decode(&share.enc_blob).unwrap();
        assert_eq!(
            sotto_core::share::open(&key, &enc_blob).unwrap(),
            b"api-token"
        );

        // With a passphrase: the fragment key alone can't decrypt; fragment + passphrase can.
        let opts = ShareOptions {
            max_views: 1,
            ttl_seconds: None,
            passphrase: Some(b"hunter2".to_vec()),
        };
        let link = create(&api, "https://app.sotto.dev", b"secret", &opts).unwrap();
        let (base, fragment) = link.split_once('#').unwrap();
        let token = base.rsplit('/').next().unwrap();
        let fragment_key: [u8; 32] = URL_SAFE_NO_PAD
            .decode(fragment)
            .unwrap()
            .try_into()
            .unwrap();
        let share = api.stored_share(token).unwrap();
        let enc_blob = b64decode(&share.enc_blob).unwrap();
        let salt: [u8; 16] = b64decode(&share.passphrase_salt.unwrap())
            .unwrap()
            .try_into()
            .unwrap();

        assert!(sotto_core::share::open(&fragment_key, &enc_blob).is_err());
        let aead_key = sotto_core::share::passphrase_key(&fragment_key, b"hunter2", &salt).unwrap();
        assert_eq!(
            sotto_core::share::open(&aead_key, &enc_blob).unwrap(),
            b"secret"
        );
    }
}
