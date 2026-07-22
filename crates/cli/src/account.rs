//! The account crypto-material bundle: the server-opaque material a new device needs after login.
//!
//! All fields are opaque to the server. [`KdfParams`] carries the (non-secret) Argon2id parameters
//! plus salt so a new device can re-derive the master key; the rest are ciphertext. The HTTP
//! upload/download lives in the sync engine (PR5b); this module just assembles the material from
//! the local store and (de)serialises the KDF parameters.

use serde::{Deserialize, Serialize};

use sotto_core::kdf;

use crate::error::{Error, Result};
use crate::store::Store;

/// Argon2id parameters + salt, serialised into the opaque `kdf_params` blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// KDF algorithm identifier (currently always `argon2id`).
    pub alg: String,
    /// Argon2id iterations (`opslimit`).
    pub opslimit: u64,
    /// Argon2id memory limit, in bytes (`memlimit`).
    pub memlimit: u64,
    /// Per-account Argon2id salt.
    pub salt: Vec<u8>,
}

impl KdfParams {
    /// The current Argon2id parameters (from the crypto core) with the given salt.
    pub fn current(salt: Vec<u8>) -> Self {
        Self {
            alg: "argon2id".into(),
            opslimit: kdf::OPSLIMIT,
            memlimit: kdf::MEMLIMIT as u64,
            salt,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        // Encoding kdf_params is not I/O; classify it as a config error to match `from_bytes`.
        serde_json::to_vec(self).map_err(|e| Error::Config(format!("encoding kdf_params: {e}")))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| Error::Config(format!("invalid kdf_params: {e}")))
    }
}

/// The four server-opaque account fields as raw bytes (the sync engine base64-encodes + uploads).
pub struct AccountMaterial {
    pub public_key: Vec<u8>,
    pub enc_private_keys: Vec<u8>,
    pub kdf_params: Vec<u8>,
    pub recovery_blob: Vec<u8>,
}

/// Assemble the local account material for upload. Errors if the identity isn't initialised.
pub fn material(store: &Store) -> Result<AccountMaterial> {
    let identity = store.get_identity()?.ok_or(Error::NoIdentity)?;
    let keys = store.get_account_keys()?.ok_or(Error::NoIdentity)?;
    Ok(AccountMaterial {
        public_key: keys.public_key,
        enc_private_keys: keys.enc_private_keys,
        kdf_params: KdfParams::current(identity.kdf_salt).to_bytes()?,
        recovery_blob: keys.recovery_blob,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use crate::session;
    use std::time::Duration;

    #[test]
    fn kdf_params_round_trip() {
        let params = KdfParams::current(vec![9u8; kdf::SALT_LEN]);
        let bytes = params.to_bytes().unwrap();
        assert_eq!(KdfParams::from_bytes(&bytes).unwrap(), params);
    }

    #[test]
    fn material_reflects_initialized_identity() {
        let store = Store::open_in_memory().unwrap();
        let kc = MemoryKeychain::default();
        session::init(&store, &kc, b"pw", Duration::from_secs(3600)).unwrap();

        let material = material(&store).unwrap();
        assert_eq!(material.public_key.len(), 32);
        assert!(!material.enc_private_keys.is_empty());
        assert!(!material.recovery_blob.is_empty());

        let parsed = KdfParams::from_bytes(&material.kdf_params).unwrap();
        assert_eq!(parsed.alg, "argon2id");
        assert_eq!(parsed.salt, store.get_identity().unwrap().unwrap().kdf_salt);
    }

    #[test]
    fn material_without_identity_errors() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(material(&store), Err(Error::NoIdentity)));
    }
}
