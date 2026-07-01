//! Crypto orchestration — sotto-core over the local [`Store`].
//!
//! The key hierarchy: the account keypair (from the session layer) opens an environment's **vault
//! key** grant; each write generates a fresh per-secret **data key** wrapped under the vault
//! key; names and values are sealed under the data key with associated data binding their
//! location (`env`, `secret`, `version`, `field`) so the store can't swap, relocate, or mix
//! blobs across secrets, environments, or versions. (AAD binding alone does not detect a
//! rollback of a whole row to an earlier consistent version — that needs separate freshness /
//! monotonic-version tracking.)
//!
//! Secret names are encrypted, so name→row resolution decrypts each row and matches — the store
//! never sees plaintext.

use sotto_core::vault as core_vault;
use sotto_core::wrap;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Error, Result};
use crate::store::{Project, SecretRow, Store};

/// Symmetric key length shared across the hierarchy.
const KEY_LEN: usize = core_vault::KEY_LEN;

/// Environments created for a new project.
pub const DEFAULT_ENVIRONMENTS: &[&str] = &["dev", "staging", "prod"];

/// An unlocked view of one environment's secrets.
///
/// Holds the environment's decrypted vault key, zeroized on drop. The account keypair opens the
/// environment's key grant and is not retained.
#[derive(ZeroizeOnDrop)]
pub struct Vault<'a> {
    #[zeroize(skip)]
    store: &'a Store,
    #[zeroize(skip)]
    env_id: String,
    vault_key: [u8; KEY_LEN],
}

impl<'a> Vault<'a> {
    /// Create a project with the [`DEFAULT_ENVIRONMENTS`], each holding a vault key granted to the
    /// account keypair.
    pub fn create_project(store: &Store, keypair: &wrap::Keypair, name: &str) -> Result<Project> {
        let project = store.create_project(name)?;
        for env in DEFAULT_ENVIRONMENTS {
            Self::create_environment(store, keypair, &project.id, env)?;
        }
        Ok(project)
    }

    /// Create one environment with a freshly generated vault key, sealed (granted) to the account's
    /// public key.
    pub fn create_environment(
        store: &Store,
        keypair: &wrap::Keypair,
        project_id: &str,
        name: &str,
    ) -> Result<()> {
        let env_id = Uuid::new_v4().to_string();
        let mut vault_key = core_vault::generate_vault_key();
        let grant = core_vault::grant_vault_key(&keypair.public, &vault_key)?;
        vault_key.zeroize();
        store.create_environment(&env_id, project_id, name, &grant)?;
        Ok(())
    }

    /// Open an environment for reading and writing.
    ///
    /// Opening the vault-key grant with `keypair` doubles as the check: a wrong keypair (or a
    /// tampered store) yields [`Error::Crypto`].
    pub fn open(
        store: &'a Store,
        keypair: &wrap::Keypair,
        project_id: &str,
        env_name: &str,
    ) -> Result<Self> {
        let env = store
            .get_environment(project_id, env_name)?
            .ok_or_else(|| Error::NotFound(format!("environment `{env_name}`")))?;
        // `enc_vault_key` now stores a vault-key grant (sealed box), not a master-wrapped key.
        let grant = &env.enc_vault_key;
        let vault_key = core_vault::open_vault_key(keypair, grant)?;
        Ok(Self {
            store,
            env_id: env.id,
            vault_key,
        })
    }

    /// Set (insert or update) a secret by name.
    ///
    /// If the name belongs to a deleted (tombstoned) secret, it is resurrected in place — the
    /// same secret id and version history continue — rather than starting a fresh secret at
    /// version 1, so versions stay monotonic per name and there are no duplicate same-name rows.
    pub fn set(&self, name: &str, value: &[u8]) -> Result<()> {
        // Prefer a live secret, then fall back to a tombstoned one to resurrect it.
        let existing = match self.find_by_name(name)? {
            Some(row) => Some(row),
            None => self.find_deleted_by_name(name)?,
        };
        match existing {
            Some(row) => {
                let new_version = row.version + 1;
                let enc = self.encrypt(&row.id, new_version, name, value);
                self.store.update_secret(
                    &row.id,
                    new_version,
                    &enc.enc_name,
                    &enc.enc_value,
                    &enc.enc_data_key,
                )
            }
            None => {
                let id = Uuid::new_v4().to_string();
                let enc = self.encrypt(&id, 1, name, value);
                self.store.insert_secret(
                    &id,
                    &self.env_id,
                    &enc.enc_name,
                    &enc.enc_value,
                    &enc.enc_data_key,
                )?;
                Ok(())
            }
        }
    }

    /// Get a secret's value by name.
    pub fn get(&self, name: &str) -> Result<Vec<u8>> {
        let row = self
            .find_by_name(name)?
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        self.decrypt_value(&row)
    }

    /// List the names of all non-deleted secrets, sorted.
    pub fn list_names(&self) -> Result<Vec<String>> {
        let mut names = self
            .store
            .list_secrets(&self.env_id)?
            .iter()
            .map(|row| self.decrypt_name(row))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// Delete a secret by name (tombstone; version history retained).
    pub fn delete(&self, name: &str) -> Result<()> {
        let row = self
            .find_by_name(name)?
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        self.store.delete_secret(&row.id)
    }

    /// All non-deleted secrets as `(name, value)` pairs, sorted by name. Unwraps each row's data
    /// key once to decrypt both fields.
    pub fn entries(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut entries = Vec::new();
        for row in self.store.list_secrets(&self.env_id)? {
            entries.push(self.decrypt_entry(&row)?);
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    // --- internals ---

    fn find_by_name(&self, name: &str) -> Result<Option<SecretRow>> {
        self.resolve_name(name, self.store.list_secrets(&self.env_id)?)
    }

    fn find_deleted_by_name(&self, name: &str) -> Result<Option<SecretRow>> {
        self.resolve_name(name, self.store.list_deleted_secrets(&self.env_id)?)
    }

    /// Match `name` against `rows` by decrypting each row's name (the store never sees plaintext).
    fn resolve_name(&self, name: &str, rows: Vec<SecretRow>) -> Result<Option<SecretRow>> {
        for row in rows {
            if self.decrypt_name(&row)? == name {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn encrypt(
        &self,
        secret_id: &str,
        version: i64,
        name: &str,
        value: &[u8],
    ) -> core_vault::EncryptedSecret {
        core_vault::encrypt_secret(
            &self.vault_key,
            &self.env_id,
            secret_id,
            version,
            name.as_bytes(),
            value,
        )
    }

    fn decrypt_name(&self, row: &SecretRow) -> Result<String> {
        let bytes = core_vault::decrypt_name(
            &self.vault_key,
            &self.env_id,
            &row.id,
            row.version,
            &row.enc_name,
            &row.enc_data_key,
        )?;
        String::from_utf8(bytes).map_err(|_| Error::Crypto)
    }

    fn decrypt_value(&self, row: &SecretRow) -> Result<Vec<u8>> {
        core_vault::decrypt_value(
            &self.vault_key,
            &self.env_id,
            &row.id,
            row.version,
            &row.enc_value,
            &row.enc_data_key,
        )
        .map_err(Into::into)
    }

    /// Decrypt both name and value from one row, unwrapping the data key a single time.
    fn decrypt_entry(&self, row: &SecretRow) -> Result<(String, Vec<u8>)> {
        let (name, value) = core_vault::decrypt_secret(
            &self.vault_key,
            &self.env_id,
            &row.id,
            row.version,
            &row.enc_name,
            &row.enc_value,
            &row.enc_data_key,
        )?;
        let name = String::from_utf8(name).map_err(|_| Error::Crypto)?;
        Ok((name, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn project() -> (Store, wrap::Keypair, String) {
        let store = Store::open_in_memory().unwrap();
        let keypair = wrap::generate_keypair();
        let project = Vault::create_project(&store, &keypair, "acme").unwrap();
        (store, keypair, project.id)
    }

    #[test]
    fn set_get_round_trip() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("DATABASE_URL", b"postgres://localhost").unwrap();
        assert_eq!(vault.get("DATABASE_URL").unwrap(), b"postgres://localhost");
    }

    #[test]
    fn entries_returns_sorted_name_value_pairs() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("B_KEY", b"b").unwrap();
        vault.set("A_KEY", b"a").unwrap();
        assert_eq!(
            vault.entries().unwrap(),
            vec![
                ("A_KEY".to_string(), b"a".to_vec()),
                ("B_KEY".to_string(), b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn set_updates_existing_in_place() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("KEY", b"v1").unwrap();
        vault.set("KEY", b"v2").unwrap();
        assert_eq!(vault.get("KEY").unwrap(), b"v2");
        // updated in place, not duplicated
        assert_eq!(vault.list_names().unwrap(), vec!["KEY"]);
    }

    #[test]
    fn list_names_is_sorted_and_resolution_is_correct() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("B_KEY", b"b").unwrap();
        vault.set("A_KEY", b"a").unwrap();
        assert_eq!(vault.list_names().unwrap(), vec!["A_KEY", "B_KEY"]);
        assert_eq!(vault.get("A_KEY").unwrap(), b"a");
        assert_eq!(vault.get("B_KEY").unwrap(), b"b");
    }

    #[test]
    fn delete_then_get_is_not_found() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("KEY", b"v").unwrap();
        vault.delete("KEY").unwrap();
        assert!(matches!(vault.get("KEY"), Err(Error::NotFound(_))));
    }

    #[test]
    fn set_after_delete_resurrects_same_secret() {
        let (store, keypair, pid) = project();
        let vault = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        vault.set("KEY", b"v1").unwrap();
        vault.delete("KEY").unwrap();
        vault.set("KEY", b"v2").unwrap();

        assert_eq!(vault.get("KEY").unwrap(), b"v2");
        assert_eq!(vault.list_names().unwrap(), vec!["KEY"]);

        // Resurrected in place: one live row whose version continued (2), not a fresh v1, and no
        // lingering tombstone.
        let env = store.get_environment(&pid, "dev").unwrap().unwrap();
        let live = store.list_secrets(&env.id).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].version, 2);
        assert!(store.list_deleted_secrets(&env.id).unwrap().is_empty());
    }

    #[test]
    fn wrong_keypair_cannot_open() {
        let (store, _keypair, pid) = project();
        let wrong = wrap::generate_keypair();
        assert!(matches!(
            Vault::open(&store, &wrong, &pid, "dev"),
            Err(Error::Crypto)
        ));
    }

    #[test]
    fn environments_are_isolated() {
        let (store, keypair, pid) = project();
        Vault::open(&store, &keypair, &pid, "dev")
            .unwrap()
            .set("KEY", b"dev-val")
            .unwrap();
        Vault::open(&store, &keypair, &pid, "prod")
            .unwrap()
            .set("KEY", b"prod-val")
            .unwrap();
        let dev = Vault::open(&store, &keypair, &pid, "dev").unwrap();
        let prod = Vault::open(&store, &keypair, &pid, "prod").unwrap();
        assert_eq!(dev.get("KEY").unwrap(), b"dev-val");
        assert_eq!(prod.get("KEY").unwrap(), b"prod-val");
    }
}
