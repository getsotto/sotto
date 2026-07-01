//! The sync engine: pull-rebase-push reconciliation of one environment's secrets.
//!
//! Secrets are opaque ciphertext whose AAD binds the (matching) `env_id`, secret id, and version,
//! so reconciliation moves blobs verbatim — never re-encrypting. `pull` applies the server snapshot
//! to the local store (server wins on a newer version, or an equal-version server-side tombstone);
//! `push` fast-forwards from a fresh snapshot, diffs local-vs-server, writes the batch at that
//! `base_revision`, and retries on a concurrency conflict (412). Project/environment names are the
//! one thing encrypted here (under the master key) for the server's zero-knowledge `enc_name`.

use std::collections::HashMap;
use std::time::Duration;

use sotto_core::{aead, kdf};

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
            apply_snapshot(store, &env.id, &snapshot)?;
            store.set_synced_revision(&env.id, snapshot.revision)?;
            Ok(snapshot.revision)
        }
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
    ensure_project_env(api, master, config.org_id.as_deref(), &project, &env)?;

    for _ in 0..MAX_SYNC_ATTEMPTS {
        // Fast-forward from the latest snapshot, then diff against it.
        let snapshot = api
            .snapshot(&env.id, None)?
            .ok_or_else(|| Error::Server("server returned no snapshot".into()))?;
        apply_snapshot(store, &env.id, &snapshot)?;
        store.set_synced_revision(&env.id, snapshot.revision)?;

        let changes = diff(store, &env.id, &snapshot)?;
        if changes.is_empty() {
            return Ok(snapshot.revision);
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
            // Someone else advanced the revision between our snapshot and write — re-pull and retry.
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

/// Upload account crypto material on first push; a 409 means it's already initialized (fine).
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

/// Idempotently create the project + environment server-side (encrypting their names). `org_id`,
/// when set, creates the project under that organization (the caller must be an admin+ of it).
fn ensure_project_env(
    api: &dyn SyncApi,
    master: &[u8; 32],
    org_id: Option<&str>,
    project: &Project,
    env: &Environment,
) -> Result<()> {
    api.create_project(&NewProject {
        id: project.id.clone(),
        enc_name: b64encode(&encrypt_name(
            master,
            &project.name,
            &project_name_aad(&project.id),
        )),
        org_id: org_id.map(str::to_string),
    })?;
    api.create_environment(
        &project.id,
        &NewEnvironment {
            id: env.id.clone(),
            enc_name: b64encode(&encrypt_name(master, &env.name, &env_name_aad(&env.id))),
            enc_vault_key: b64encode(&env.enc_vault_key),
        },
    )
}

/// Apply a server snapshot to the local store: server wins on a strictly newer version, or on an
/// equal-version tombstone the server introduced.
fn apply_snapshot(store: &Store, env_id: &str, snapshot: &Snapshot) -> Result<()> {
    for remote in &snapshot.secrets {
        let local = store.find_secret(env_id, &remote.id)?;
        let apply = match &local {
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

fn encrypt_name(master: &[u8; 32], name: &str, aad: &str) -> Vec<u8> {
    aead::seal(master, name.as_bytes(), aad.as_bytes())
}

fn project_name_aad(project_id: &str) -> String {
    format!("sotto/v1/project-name|id={project_id}")
}

fn env_name_aad(env_id: &str) -> String {
    format!("sotto/v1/env-name|id={env_id}")
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

/// Reconstruct the local project + environments from the server (decrypting env names under the
/// master key). Existing local rows are left untouched. Run after [`restore_account`], before
/// [`pull`], on a new device.
pub fn pull_environments(
    api: &dyn SyncApi,
    store: &Store,
    master: &[u8; 32],
    config: &Config,
) -> Result<()> {
    if store.get_project(&config.project_id)?.is_none() {
        store.create_project_with_id(&config.project_id, &config.project)?;
    }
    for env in api.list_environments(&config.project_id)? {
        if store.find_environment(&env.id)?.is_some() {
            continue;
        }
        let name = decrypt_name(master, &b64decode(&env.enc_name)?, &env_name_aad(&env.id))?;
        store.create_environment(
            &env.id,
            &config.project_id,
            &name,
            &b64decode(&env.enc_vault_key)?,
        )?;
    }
    Ok(())
}

fn decrypt_name(master: &[u8; 32], ciphertext: &[u8], aad: &str) -> Result<String> {
    let bytes = aead::open(master, ciphertext, aad.as_bytes())?;
    String::from_utf8(bytes).map_err(|_| Error::Crypto)
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

    struct EnvState {
        project_id: String,
        enc_name: String,
        enc_vault_key: String,
        revision: i64,
        secrets: HashMap<String, ServerSecret>,
    }

    struct MemberRec {
        role: String,
        public_key: Option<Vec<u8>>,
    }

    struct OrgState {
        enc_name: String,
        members: HashMap<String, MemberRec>,
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
                return Err(Error::Conflict("account already initialized".into()));
            }
            s.account = Some(bundle.clone());
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
            self.state
                .borrow_mut()
                .envs
                .entry(env.id.clone())
                .or_insert(EnvState {
                    project_id: project_id.to_string(),
                    enc_name: env.enc_name.clone(),
                    enc_vault_key: env.enc_vault_key.clone(),
                    revision: 0,
                    secrets: HashMap::new(),
                });
            Ok(())
        }

        fn list_environments(
            &self,
            project_id: &str,
        ) -> Result<Vec<super::super::api::EnvironmentInfo>> {
            let s = self.state.borrow();
            let mut envs: Vec<_> = s
                .envs
                .iter()
                .filter(|(_, e)| e.project_id == project_id)
                .map(|(id, e)| super::super::api::EnvironmentInfo {
                    id: id.clone(),
                    enc_name: e.enc_name.clone(),
                    enc_vault_key: e.enc_vault_key.clone(),
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
                    })
                })
                .collect();
            orgs.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(orgs)
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
                .ok_or_else(|| Error::NotFound("organization not found".into()))?;
            org.members.entry(user_id.clone()).or_insert(MemberRec {
                role: "member".into(),
                public_key: public_key.clone(),
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
                .ok_or_else(|| Error::NotFound("organization not found".into()))?;
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
    }

    /// A store with an initialized account + a project (dev/staging/prod) and its config.
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
        // A real second device has its own initialized identity/account material (its push's
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
    /// account material is consistent with the secrets — required to test reconstruction.
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
        let org_id = team::create_org(&api, &master_a, "acme").unwrap();
        assert_eq!(
            team::invite(&api, &org_id, "bob@example.test")
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
            "acme",
            "dev",
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
            "acme",
            "dev",
            Some(&org_id)
        )
        .is_err());
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
