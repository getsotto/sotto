//! OS keychain abstraction.
//!
//! A small byte-oriented key→value store, behind a trait so the session logic stays unit-testable
//! with [`MemoryKeychain`] while real runs use [`OsKeychain`] (the platform credential store via
//! the `keyring` crate). The CLI stores two things here: the persistent secret key and the
//! TTL-bounded session (the cached master key + its expiry).

use crate::error::{Error, Result};

/// A byte-oriented secret store keyed by a short name.
pub trait Keychain {
    /// Fetch a stored value, or `None` if absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Store (or replace) a value.
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
    /// Remove a value; a no-op if it doesn't exist.
    fn delete(&self, key: &str) -> Result<()>;
}

/// The platform credential store (macOS Keychain, Windows Credential Manager, Linux Secret
/// Service) via the `keyring` crate.
pub struct OsKeychain {
    service: String,
}

impl OsKeychain {
    /// Create a keychain scoped to the given service name (e.g. `"sotto"`).
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(|e| Error::Keychain(e.to_string()))
    }
}

impl Default for OsKeychain {
    fn default() -> Self {
        Self::new("sotto")
    }
}

impl Keychain for OsKeychain {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.entry(key)?.get_secret() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Keychain(e.to_string())),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        self.entry(key)?
            .set_secret(value)
            .map_err(|e| Error::Keychain(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Keychain(e.to_string())),
        }
    }
}

/// An in-memory keychain for tests. Not for production use — it offers no at-rest protection.
#[derive(Default)]
pub struct MemoryKeychain {
    items: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl Keychain for MemoryKeychain {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.items.lock().expect("lock").get(key).cloned())
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        self.items
            .lock()
            .expect("lock")
            .insert(key.to_string(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.items.lock().expect("lock").remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_keychain_get_set_delete() {
        let kc = MemoryKeychain::default();
        assert!(kc.get("k").unwrap().is_none());
        kc.set("k", b"value").unwrap();
        assert_eq!(kc.get("k").unwrap().unwrap(), b"value");
        kc.set("k", b"updated").unwrap();
        assert_eq!(kc.get("k").unwrap().unwrap(), b"updated");
        kc.delete("k").unwrap();
        assert!(kc.get("k").unwrap().is_none());
        // delete of a missing key is a no-op
        kc.delete("k").unwrap();
    }
}
