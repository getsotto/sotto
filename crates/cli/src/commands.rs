//! The command layer — secret operations over the store, vault, and session.
//!
//! These methods hold **no IO**: no prompting, no printing. The binary resolves inputs (config,
//! password) and renders results; this layer is the testable core (driven with a mock keychain
//! in tests). Identity/session setup (`init`/`unlock`/`lock`) lives in [`crate::session`].

use crate::config::Config;
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::session::{self, MasterKey};
use crate::store::Store;
use crate::vault::Vault;

/// How one key compares across two environments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    /// Present only in the left environment.
    OnlyLeft,
    /// Present only in the right environment.
    OnlyRight,
    /// Present in both with different values.
    Differs,
    /// Present in both with the same value.
    Equal,
}

/// One key of an environment diff, with both decrypted values (display is the caller's choice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub name: String,
    pub status: DiffStatus,
    pub left: Option<Vec<u8>>,
    pub right: Option<Vec<u8>>,
}

/// What an `env copy` would (or did) change, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyPlan {
    /// Keys new to the destination.
    pub create: Vec<String>,
    /// Keys whose destination value differs and gets overwritten.
    pub update: Vec<String>,
    /// Keys already equal — never rewritten (no pointless version bumps).
    pub unchanged: Vec<String>,
}

/// A snapshot of identity / session / project state for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Whether an identity has been set up.
    pub initialized: bool,
    /// Whether the session is currently unlocked.
    pub unlocked: bool,
    /// `(project name, active environment)` when a config is present.
    pub project: Option<(String, String)>,
}

/// Secret operations bound to a store and keychain.
pub struct App<'a> {
    store: &'a Store,
    keychain: &'a dyn Keychain,
}

impl<'a> App<'a> {
    pub fn new(store: &'a Store, keychain: &'a dyn Keychain) -> Self {
        Self { store, keychain }
    }

    /// Set (insert or update) a secret in the configured environment.
    pub fn set(&self, config: &Config, name: &str, value: &[u8]) -> Result<()> {
        self.vault(config)?.set(name, value)
    }

    /// Get a secret's value from the configured environment.
    pub fn get(&self, config: &Config, name: &str) -> Result<Vec<u8>> {
        self.vault(config)?.get(name)
    }

    /// List secret names in the configured environment, sorted.
    pub fn list(&self, config: &Config) -> Result<Vec<String>> {
        self.vault(config)?.list_names()
    }

    /// Remove a secret from the configured environment.
    pub fn remove(&self, config: &Config, name: &str) -> Result<()> {
        self.vault(config)?.delete(name)
    }

    /// All secrets in the configured environment as `(name, value)` pairs (for `run`/`export`).
    pub fn entries(&self, config: &Config) -> Result<Vec<(String, Vec<u8>)>> {
        self.vault(config)?.entries()
    }

    /// List the project's environments (names are plaintext metadata; no unlock needed).
    pub fn env_list(&self, config: &Config) -> Result<Vec<String>> {
        self.store.list_environments(&config.project_id)
    }

    /// Compare two environments of the configured project, key by key. Returns one entry per key
    /// in the union of both environments, sorted by name, with both (decrypted) values so the
    /// caller decides whether to display them (`--reveal`) or only the markers.
    pub fn env_diff(&self, config: &Config, left: &str, right: &str) -> Result<Vec<DiffEntry>> {
        let left_entries = self.entries_for(config, left)?;
        let right_entries: std::collections::BTreeMap<String, Vec<u8>> =
            self.entries_for(config, right)?.into_iter().collect();
        let left_entries: std::collections::BTreeMap<String, Vec<u8>> =
            left_entries.into_iter().collect();

        let mut names: Vec<&String> = left_entries.keys().chain(right_entries.keys()).collect();
        names.sort();
        names.dedup();

        Ok(names
            .into_iter()
            .map(|name| {
                let left = left_entries.get(name).cloned();
                let right = right_entries.get(name).cloned();
                let status = match (&left, &right) {
                    (Some(l), Some(r)) if l == r => DiffStatus::Equal,
                    (Some(_), Some(_)) => DiffStatus::Differs,
                    (Some(_), None) => DiffStatus::OnlyLeft,
                    (None, Some(_)) => DiffStatus::OnlyRight,
                    (None, None) => unreachable!("name came from one of the two maps"),
                };
                DiffEntry {
                    name: name.clone(),
                    status,
                    left,
                    right,
                }
            })
            .collect())
    }

    /// Plan (and with `apply`, perform) a copy of `src`'s secrets onto `dst` — the promotion flow.
    /// Add/update only: keys equal in both are skipped, and keys present only in `dst` are never
    /// deleted (no pruning — that's the prod-overwrite footgun this design avoids). Values are
    /// re-encrypted under `dst`'s vault key through the normal set path, so versions and history
    /// behave as if each secret had been set by hand.
    pub fn env_copy(&self, config: &Config, src: &str, dst: &str, apply: bool) -> Result<CopyPlan> {
        if src == dst {
            return Err(Error::Input(
                "source and destination environments are the same".into(),
            ));
        }
        let mut plan = CopyPlan::default();
        let src_entries = self.entries_for(config, src)?;
        let dst_vault = self.vault_for(config, dst)?;
        let dst_map: std::collections::BTreeMap<String, Vec<u8>> =
            dst_vault.entries()?.into_iter().collect();

        for (name, value) in src_entries {
            match dst_map.get(&name) {
                Some(existing) if *existing == value => plan.unchanged.push(name),
                Some(_) => {
                    if apply {
                        dst_vault.set(&name, &value)?;
                    }
                    plan.update.push(name);
                }
                None => {
                    if apply {
                        dst_vault.set(&name, &value)?;
                    }
                    plan.create.push(name);
                }
            }
        }
        Ok(plan)
    }

    /// All secrets of one named environment (not the config's active one).
    fn entries_for(&self, config: &Config, env: &str) -> Result<Vec<(String, Vec<u8>)>> {
        self.vault_for(config, env)?.entries()
    }

    /// Report identity / session / project state.
    pub fn status(&self, config: Option<&Config>) -> Result<Status> {
        Ok(Status {
            initialized: self.store.get_identity()?.is_some(),
            unlocked: session::current_master_key(self.keychain)?.is_some(),
            project: config.map(|c| (c.project.clone(), c.environment.clone())),
        })
    }

    /// Open the vault for the configured project/environment, requiring an unlocked session.
    fn vault(&self, config: &Config) -> Result<Vault<'a>> {
        self.vault_for(config, &config.environment)
    }

    /// Open the vault for a named environment of the configured project.
    fn vault_for(&self, config: &Config, env: &str) -> Result<Vault<'a>> {
        let master = self.require_unlocked()?;
        let keypair = session::account_keypair(self.store, &master)?;
        Vault::open(self.store, &keypair, &config.project_id, env)
    }

    fn require_unlocked(&self) -> Result<MasterKey> {
        session::current_master_key(self.keychain)?.ok_or(Error::Locked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use std::time::Duration;

    /// An unlocked store + keychain with one project, plus its config.
    fn unlocked() -> (Store, MemoryKeychain, Config) {
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(&store, &keychain, b"pw", Duration::from_secs(3600)).unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        (store, keychain, config)
    }

    #[test]
    fn set_get_list_remove_flow() {
        let (store, keychain, config) = unlocked();
        let app = App::new(&store, &keychain);

        app.set(&config, "DATABASE_URL", b"postgres://x").unwrap();
        app.set(&config, "API_KEY", b"sk-123").unwrap();
        assert_eq!(app.get(&config, "DATABASE_URL").unwrap(), b"postgres://x");
        assert_eq!(app.list(&config).unwrap(), vec!["API_KEY", "DATABASE_URL"]);

        app.remove(&config, "API_KEY").unwrap();
        assert_eq!(app.list(&config).unwrap(), vec!["DATABASE_URL"]);
        assert!(matches!(
            app.get(&config, "API_KEY"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn secret_commands_require_unlock() {
        let (store, keychain, config) = unlocked();
        session::lock(&keychain).unwrap();
        let app = App::new(&store, &keychain);
        assert!(matches!(app.set(&config, "X", b"v"), Err(Error::Locked)));
        assert!(matches!(app.get(&config, "X"), Err(Error::Locked)));
        assert!(matches!(app.list(&config), Err(Error::Locked)));
        assert!(matches!(app.entries(&config), Err(Error::Locked)));
    }

    #[test]
    fn env_list_and_status() {
        let (store, keychain, config) = unlocked();
        let app = App::new(&store, &keychain);

        assert_eq!(
            app.env_list(&config).unwrap(),
            vec!["dev", "prod", "staging"]
        );

        let status = app.status(Some(&config)).unwrap();
        assert!(status.initialized);
        assert!(status.unlocked);
        assert_eq!(status.project, Some(("acme".into(), "dev".into())));
    }

    #[test]
    fn status_without_config_or_session() {
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        let app = App::new(&store, &keychain);
        let status = app.status(None).unwrap();
        assert!(!status.initialized);
        assert!(!status.unlocked);
        assert_eq!(status.project, None);
    }

    /// Seed dev + staging with an overlapping key set for the diff/copy tests:
    /// SHARED (equal), CHANGED (differs), DEV_ONLY, STG_ONLY.
    fn seeded_for_promotion() -> (Store, MemoryKeychain, Config) {
        let (store, keychain, config) = unlocked();
        let app = App::new(&store, &keychain);
        let dev = config.clone();
        let stg = Config {
            environment: "staging".into(),
            ..config.clone()
        };
        app.set(&dev, "SHARED", b"same").unwrap();
        app.set(&stg, "SHARED", b"same").unwrap();
        app.set(&dev, "CHANGED", b"dev-value").unwrap();
        app.set(&stg, "CHANGED", b"stg-value").unwrap();
        app.set(&dev, "DEV_ONLY", b"d").unwrap();
        app.set(&stg, "STG_ONLY", b"s").unwrap();
        (store, keychain, config)
    }

    #[test]
    fn env_diff_reports_presence_and_differences() {
        let (store, keychain, config) = seeded_for_promotion();
        let app = App::new(&store, &keychain);

        let diff = app.env_diff(&config, "dev", "staging").unwrap();
        let by_name: std::collections::HashMap<&str, &DiffEntry> =
            diff.iter().map(|e| (e.name.as_str(), e)).collect();
        assert_eq!(diff.len(), 4);
        assert_eq!(by_name["SHARED"].status, DiffStatus::Equal);
        assert_eq!(by_name["CHANGED"].status, DiffStatus::Differs);
        assert_eq!(by_name["CHANGED"].left.as_deref(), Some(&b"dev-value"[..]));
        assert_eq!(by_name["CHANGED"].right.as_deref(), Some(&b"stg-value"[..]));
        assert_eq!(by_name["DEV_ONLY"].status, DiffStatus::OnlyLeft);
        assert_eq!(by_name["STG_ONLY"].status, DiffStatus::OnlyRight);
        // Sorted by name.
        let names: Vec<&str> = diff.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["CHANGED", "DEV_ONLY", "SHARED", "STG_ONLY"]);
    }

    #[test]
    fn env_copy_dry_run_plans_without_writing() {
        let (store, keychain, config) = seeded_for_promotion();
        let app = App::new(&store, &keychain);

        let plan = app.env_copy(&config, "dev", "staging", false).unwrap();
        assert_eq!(plan.create, vec!["DEV_ONLY"]);
        assert_eq!(plan.update, vec!["CHANGED"]);
        assert_eq!(plan.unchanged, vec!["SHARED"]);

        // Nothing was written: staging still has its own value and no DEV_ONLY.
        let stg = Config {
            environment: "staging".into(),
            ..config
        };
        assert_eq!(app.get(&stg, "CHANGED").unwrap(), b"stg-value");
        assert!(matches!(app.get(&stg, "DEV_ONLY"), Err(Error::NotFound(_))));
    }

    #[test]
    fn env_copy_applies_without_pruning() {
        let (store, keychain, config) = seeded_for_promotion();
        let app = App::new(&store, &keychain);

        let plan = app.env_copy(&config, "dev", "staging", true).unwrap();
        assert_eq!(plan.create, vec!["DEV_ONLY"]);
        assert_eq!(plan.update, vec!["CHANGED"]);

        let stg = Config {
            environment: "staging".into(),
            ..config.clone()
        };
        assert_eq!(app.get(&stg, "CHANGED").unwrap(), b"dev-value");
        assert_eq!(app.get(&stg, "DEV_ONLY").unwrap(), b"d");
        // Promotion never prunes: the destination-only key survives.
        assert_eq!(app.get(&stg, "STG_ONLY").unwrap(), b"s");
        // The source is untouched.
        assert!(matches!(
            app.get(&config, "STG_ONLY"),
            Err(Error::NotFound(_))
        ));

        // Re-copying is a no-op: everything is now unchanged.
        let again = app.env_copy(&config, "dev", "staging", true).unwrap();
        assert!(again.create.is_empty() && again.update.is_empty());
        assert_eq!(again.unchanged.len(), 3);
    }

    #[test]
    fn env_copy_rejects_same_env_and_missing_env() {
        let (store, keychain, config) = seeded_for_promotion();
        let app = App::new(&store, &keychain);
        assert!(matches!(
            app.env_copy(&config, "dev", "dev", false),
            Err(Error::Input(_))
        ));
        assert!(matches!(
            app.env_copy(&config, "dev", "nope", false),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            app.env_diff(&config, "nope", "dev"),
            Err(Error::NotFound(_))
        ));
    }
}
