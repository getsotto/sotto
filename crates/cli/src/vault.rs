//! Crypto orchestration — sotto-core over the local [`Store`].
//!
//! The key hierarchy: a master key (supplied by the session layer) unwraps an environment's
//! **vault key**; each write generates a fresh per-secret **data key** wrapped under the vault
//! key; names and values are sealed under the data key with associated data binding their
//! location (`env`, `secret`, `version`, `field`) so the store can't swap or roll back a blob.
//!
//! Secret names are encrypted, so name→row resolution decrypts each row and matches — the store
//! never sees plaintext.

use sotto_core::{aead, random, wrap};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Error, Result};
use crate::store::{Project, SecretRow, Store};

/// Symmetric key length shared across the hierarchy.
const KEY_LEN: usize = 32;

/// Environments created for a new project.
pub const DEFAULT_ENVIRONMENTS: &[&str] = &["dev", "staging", "prod"];

/// An unlocked view of one environment's secrets.
///
/// Holds the environment's decrypted vault key, zeroized on drop. The master key is used only to
/// construct the vault and is not retained.
#[derive(ZeroizeOnDrop)]
pub struct Vault<'a> {
    #[zeroize(skip)]
    store: &'a Store,
    #[zeroize(skip)]
    env_id: String,
    vault_key: [u8; KEY_LEN],
}

impl<'a> Vault<'a> {
    /// Create a project with the [`DEFAULT_ENVIRONMENTS`], each holding a fresh wrapped vault key.
    pub fn create_project(
        store: &Store,
        master_key: &[u8; KEY_LEN],
        name: &str,
    ) -> Result<Project> {
        let project = store.create_project(name)?;
        for env in DEFAULT_ENVIRONMENTS {
            Self::create_environment(store, master_key, &project.id, env)?;
        }
        Ok(project)
    }

    /// Create one environment with a freshly generated, master-key-wrapped vault key.
    pub fn create_environment(
        store: &Store,
        master_key: &[u8; KEY_LEN],
        project_id: &str,
        name: &str,
    ) -> Result<()> {
        let env_id = Uuid::new_v4().to_string();
        let mut vault_key = random::bytes::<KEY_LEN>();
        let enc_vault_key =
            wrap::wrap_key(master_key, &vault_key, vault_key_aad(&env_id).as_bytes());
        vault_key.zeroize();
        store.create_environment(&env_id, project_id, name, &enc_vault_key)?;
        Ok(())
    }

    /// Open an environment for reading and writing.
    ///
    /// Unwrapping the vault key with `master_key` doubles as the unlock check: a wrong master key
    /// (or a tampered store) yields [`Error::Crypto`].
    pub fn open(
        store: &'a Store,
        master_key: &[u8; KEY_LEN],
        project_id: &str,
        env_name: &str,
    ) -> Result<Self> {
        let env = store
            .get_environment(project_id, env_name)?
            .ok_or_else(|| Error::NotFound(format!("environment `{env_name}`")))?;
        let vault_key = wrap::unwrap_key(
            master_key,
            &env.enc_vault_key,
            vault_key_aad(&env.id).as_bytes(),
        )?;
        Ok(Self {
            store,
            env_id: env.id,
            vault_key,
        })
    }

    /// Set (insert or update) a secret by name.
    pub fn set(&self, name: &str, value: &[u8]) -> Result<()> {
        match self.find_by_name(name)? {
            Some(row) => {
                let new_version = row.version + 1;
                let enc = self.encrypt(&row.id, new_version, name, value);
                self.store
                    .update_secret(&row.id, new_version, &enc.name, &enc.value, &enc.data_key)
            }
            None => {
                let id = Uuid::new_v4().to_string();
                let enc = self.encrypt(&id, 1, name, value);
                self.store.insert_secret(
                    &id,
                    &self.env_id,
                    &enc.name,
                    &enc.value,
                    &enc.data_key,
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

    // --- internals ---

    fn find_by_name(&self, name: &str) -> Result<Option<SecretRow>> {
        for row in self.store.list_secrets(&self.env_id)? {
            if self.decrypt_name(&row)? == name {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn encrypt(&self, secret_id: &str, version: i64, name: &str, value: &[u8]) -> EncryptedSecret {
        let mut data_key = random::bytes::<KEY_LEN>();
        let name = aead::seal(
            &data_key,
            name.as_bytes(),
            name_aad(&self.env_id, secret_id, version).as_bytes(),
        );
        let value = aead::seal(
            &data_key,
            value,
            value_aad(&self.env_id, secret_id, version).as_bytes(),
        );
        let data_key_ct = wrap::wrap_key(
            &self.vault_key,
            &data_key,
            data_key_aad(&self.env_id, secret_id, version).as_bytes(),
        );
        data_key.zeroize();
        EncryptedSecret {
            name,
            value,
            data_key: data_key_ct,
        }
    }

    fn data_key(&self, row: &SecretRow) -> Result<[u8; KEY_LEN]> {
        wrap::unwrap_key(
            &self.vault_key,
            &row.enc_data_key,
            data_key_aad(&self.env_id, &row.id, row.version).as_bytes(),
        )
        .map_err(Into::into)
    }

    fn decrypt_name(&self, row: &SecretRow) -> Result<String> {
        let mut dk = self.data_key(row)?;
        let bytes = aead::open(
            &dk,
            &row.enc_name,
            name_aad(&self.env_id, &row.id, row.version).as_bytes(),
        );
        dk.zeroize();
        String::from_utf8(bytes?).map_err(|_| Error::Crypto)
    }

    fn decrypt_value(&self, row: &SecretRow) -> Result<Vec<u8>> {
        let mut dk = self.data_key(row)?;
        let value = aead::open(
            &dk,
            &row.enc_value,
            value_aad(&self.env_id, &row.id, row.version).as_bytes(),
        );
        dk.zeroize();
        Ok(value?)
    }
}

/// Ciphertext components produced by one secret write.
struct EncryptedSecret {
    name: Vec<u8>,
    value: Vec<u8>,
    data_key: Vec<u8>,
}

fn vault_key_aad(env_id: &str) -> String {
    format!("sotto/v1/vaultkey|env={env_id}")
}
fn data_key_aad(env_id: &str, secret_id: &str, version: i64) -> String {
    format!("sotto/v1/datakey|env={env_id}|secret={secret_id}|ver={version}")
}
fn name_aad(env_id: &str, secret_id: &str, version: i64) -> String {
    format!("sotto/v1/name|env={env_id}|secret={secret_id}|ver={version}")
}
fn value_aad(env_id: &str, secret_id: &str, version: i64) -> String {
    format!("sotto/v1/value|env={env_id}|secret={secret_id}|ver={version}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    const MASTER: [u8; KEY_LEN] = [0x42; KEY_LEN];

    fn project() -> (Store, String) {
        let store = Store::open_in_memory().unwrap();
        let project = Vault::create_project(&store, &MASTER, "acme").unwrap();
        (store, project.id)
    }

    #[test]
    fn set_get_round_trip() {
        let (store, pid) = project();
        let vault = Vault::open(&store, &MASTER, &pid, "dev").unwrap();
        vault.set("DATABASE_URL", b"postgres://localhost").unwrap();
        assert_eq!(vault.get("DATABASE_URL").unwrap(), b"postgres://localhost");
    }

    #[test]
    fn set_updates_existing_in_place() {
        let (store, pid) = project();
        let vault = Vault::open(&store, &MASTER, &pid, "dev").unwrap();
        vault.set("KEY", b"v1").unwrap();
        vault.set("KEY", b"v2").unwrap();
        assert_eq!(vault.get("KEY").unwrap(), b"v2");
        // updated in place, not duplicated
        assert_eq!(vault.list_names().unwrap(), vec!["KEY"]);
    }

    #[test]
    fn list_names_is_sorted_and_resolution_is_correct() {
        let (store, pid) = project();
        let vault = Vault::open(&store, &MASTER, &pid, "dev").unwrap();
        vault.set("B_KEY", b"b").unwrap();
        vault.set("A_KEY", b"a").unwrap();
        assert_eq!(vault.list_names().unwrap(), vec!["A_KEY", "B_KEY"]);
        assert_eq!(vault.get("A_KEY").unwrap(), b"a");
        assert_eq!(vault.get("B_KEY").unwrap(), b"b");
    }

    #[test]
    fn delete_then_get_is_not_found() {
        let (store, pid) = project();
        let vault = Vault::open(&store, &MASTER, &pid, "dev").unwrap();
        vault.set("KEY", b"v").unwrap();
        vault.delete("KEY").unwrap();
        assert!(matches!(vault.get("KEY"), Err(Error::NotFound(_))));
    }

    #[test]
    fn wrong_master_key_cannot_open() {
        let (store, pid) = project();
        let wrong = [0x99; KEY_LEN];
        assert!(matches!(
            Vault::open(&store, &wrong, &pid, "dev"),
            Err(Error::Crypto)
        ));
    }

    #[test]
    fn environments_are_isolated() {
        let (store, pid) = project();
        Vault::open(&store, &MASTER, &pid, "dev")
            .unwrap()
            .set("KEY", b"dev-val")
            .unwrap();
        Vault::open(&store, &MASTER, &pid, "prod")
            .unwrap()
            .set("KEY", b"prod-val")
            .unwrap();
        let dev = Vault::open(&store, &MASTER, &pid, "dev").unwrap();
        let prod = Vault::open(&store, &MASTER, &pid, "prod").unwrap();
        assert_eq!(dev.get("KEY").unwrap(), b"dev-val");
        assert_eq!(prod.get("KEY").unwrap(), b"prod-val");
    }
}
