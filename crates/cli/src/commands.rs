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

    /// List the project's environments (names are plaintext metadata; no unlock needed).
    pub fn env_list(&self, config: &Config) -> Result<Vec<String>> {
        self.store.list_environments(&config.project_id)
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
        let master = self.require_unlocked()?;
        Vault::open(
            self.store,
            master.as_bytes(),
            &config.project_id,
            &config.environment,
        )
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
        let project = Vault::create_project(&store, master.as_bytes(), "acme").unwrap();
        let config = Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
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
}
