//! WASM bindings to the Sotto crypto core for the web client.
//!
//! The browser runs the SAME crypto core as the CLI, compiled to WASM — one implementation,
//! one audit surface. The cross-implementation gate (`tests/cross_impl.rs`) proves this build
//! decrypts native-produced ciphertext byte-for-byte.

use wasm_bindgen::prelude::*;

/// The crypto scheme version baked into this build. The gate asserts it equals the native
/// core's `SCHEME_VERSION`, so the two builds can never silently diverge.
#[wasm_bindgen]
pub fn scheme_version() -> u8 {
    sotto_core::SCHEME_VERSION
}

fn key32(key: &[u8]) -> Result<[u8; 32], JsError> {
    key.try_into()
        .map_err(|_| JsError::new("key must be 32 bytes"))
}

/// Encrypt `plaintext` under a 32-byte `key`, binding `aad`; returns a versioned envelope.
#[wasm_bindgen]
pub fn aead_seal(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(sotto_core::aead::seal(&key32(key)?, plaintext, aad))
}

/// Decrypt a versioned envelope under a 32-byte `key`, verifying `aad`.
#[wasm_bindgen]
pub fn aead_open(key: &[u8], envelope: &[u8], aad: &[u8]) -> Result<Vec<u8>, JsError> {
    sotto_core::aead::open(&key32(key)?, envelope, aad).map_err(|e| JsError::new(&e.to_string()))
}

/// Seal a secret for a share link under a 32-byte share key; returns a versioned envelope.
#[wasm_bindgen]
pub fn share_seal(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(sotto_core::share::seal(&key32(key)?, plaintext))
}

/// Open a share envelope under a 32-byte share key (the recipient's decrypt path).
#[wasm_bindgen]
pub fn share_open(key: &[u8], envelope: &[u8]) -> Result<Vec<u8>, JsError> {
    sotto_core::share::open(&key32(key)?, envelope).map_err(|e| JsError::new(&e.to_string()))
}

/// Derive the AEAD key for a passphrase-protected share from the fragment key + passphrase + salt.
#[wasm_bindgen]
pub fn share_passphrase_key(
    fragment_key: &[u8],
    passphrase: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, JsError> {
    let salt: [u8; 16] = salt
        .try_into()
        .map_err(|_| JsError::new("salt must be 16 bytes"))?;
    sotto_core::share::passphrase_key(&key32(fragment_key)?, passphrase, &salt)
        .map(|k| k.to_vec())
        .map_err(|e| JsError::new(&e.to_string()))
}

// --- vault: derive the master key, then the environment key hierarchy (the in-browser vault) ---

/// Derive the 32-byte master key from the password, secret key, and 16-byte salt (Argon2id in WASM).
#[wasm_bindgen]
pub fn kdf_derive_master_key(
    password: &[u8],
    secret_key: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, JsError> {
    let salt: [u8; 16] = salt
        .try_into()
        .map_err(|_| JsError::new("salt must be 16 bytes"))?;
    sotto_core::kdf::derive_master_key(password, secret_key, &salt)
        .map(|k| k.to_vec())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Seal a vault key to a member's public key (a grant), for creating/sharing an environment.
#[wasm_bindgen]
pub fn vault_grant_key(recipient_public: &[u8], vault_key: &[u8]) -> Result<Vec<u8>, JsError> {
    let recipient: [u8; 32] = recipient_public
        .try_into()
        .map_err(|_| JsError::new("public key must be 32 bytes"))?;
    sotto_core::vault::grant_vault_key(&recipient, &key32(vault_key)?)
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Open an environment vault-key grant: recover the account keypair from `enc_private_keys` under
/// the master key, then unseal the grant. (The private key never leaves WASM.)
#[wasm_bindgen]
pub fn vault_open_grant(
    master_key: &[u8],
    enc_private_keys: &[u8],
    grant: &[u8],
) -> Result<Vec<u8>, JsError> {
    sotto_core::vault::open_vault_grant(&key32(master_key)?, enc_private_keys, grant)
        .map(|k| k.to_vec())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Rewrap a secret's data key from the old vault key to a new one — the browser side of key
/// rotation. Ciphertext is untouched; only the wrapping changes (same `(env, secret, version)`
/// binding), exactly as the CLI does it.
#[wasm_bindgen]
pub fn vault_rewrap_data_key(
    old_vault_key: &[u8],
    new_vault_key: &[u8],
    env_id: &str,
    secret_id: &str,
    version: i32,
    enc_data_key: &[u8],
) -> Result<Vec<u8>, JsError> {
    sotto_core::vault::rewrap_data_key(
        &key32(old_vault_key)?,
        &key32(new_vault_key)?,
        env_id,
        secret_id,
        version.into(),
        enc_data_key,
    )
    .map_err(|e| JsError::new(&e.to_string()))
}

/// The ciphertext of an encrypted secret, exposed to JS with getters.
#[wasm_bindgen]
pub struct EncryptedSecret {
    inner: sotto_core::vault::EncryptedSecret,
}

#[wasm_bindgen]
impl EncryptedSecret {
    #[wasm_bindgen(getter)]
    pub fn enc_name(&self) -> Vec<u8> {
        self.inner.enc_name.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn enc_value(&self) -> Vec<u8> {
        self.inner.enc_value.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn enc_data_key(&self) -> Vec<u8> {
        self.inner.enc_data_key.clone()
    }
}

/// Encrypt a secret's name + value under a fresh data key wrapped by the vault key.
#[wasm_bindgen]
pub fn vault_encrypt_secret(
    vault_key: &[u8],
    env_id: &str,
    secret_id: &str,
    version: i32,
    name: &[u8],
    value: &[u8],
) -> Result<EncryptedSecret, JsError> {
    Ok(EncryptedSecret {
        inner: sotto_core::vault::encrypt_secret(
            &key32(vault_key)?,
            env_id,
            secret_id,
            version.into(),
            name,
            value,
        ),
    })
}

/// Decrypt a secret's name.
#[wasm_bindgen]
pub fn vault_decrypt_name(
    vault_key: &[u8],
    env_id: &str,
    secret_id: &str,
    version: i32,
    enc_name: &[u8],
    enc_data_key: &[u8],
) -> Result<Vec<u8>, JsError> {
    sotto_core::vault::decrypt_name(
        &key32(vault_key)?,
        env_id,
        secret_id,
        version.into(),
        enc_name,
        enc_data_key,
    )
    .map_err(|e| JsError::new(&e.to_string()))
}

/// Decrypt a secret's value.
#[wasm_bindgen]
pub fn vault_decrypt_value(
    vault_key: &[u8],
    env_id: &str,
    secret_id: &str,
    version: i32,
    enc_value: &[u8],
    enc_data_key: &[u8],
) -> Result<Vec<u8>, JsError> {
    sotto_core::vault::decrypt_value(
        &key32(vault_key)?,
        env_id,
        secret_id,
        version.into(),
        enc_value,
        enc_data_key,
    )
    .map_err(|e| JsError::new(&e.to_string()))
}

/// Decode a human key string (e.g. a pasted `SK1-…` secret key) to its raw bytes, verifying the
/// prefix, version, and checksum.
#[wasm_bindgen]
pub fn format_decode_key(prefix: &str, version: u8, s: &str) -> Result<Vec<u8>, JsError> {
    sotto_core::format::decode_key(prefix, version, s).map_err(|e| JsError::new(&e.to_string()))
}

// --- metadata display names (single-source scheme in `sotto_core::names`) ---

/// Decrypt an organization's name (org key for shared orgs; older orgs used the creator's master).
#[wasm_bindgen]
pub fn name_decrypt_org(key: &[u8], org_id: &str, ciphertext: &[u8]) -> Result<Vec<u8>, JsError> {
    sotto_core::names::decrypt_org_name(&key32(key)?, org_id, ciphertext)
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Decrypt a project's name (master key for personal projects, org key for org projects).
#[wasm_bindgen]
pub fn name_decrypt_project(
    key: &[u8],
    project_id: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>, JsError> {
    sotto_core::names::decrypt_project_name(&key32(key)?, project_id, ciphertext)
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Decrypt an environment's name (master key for personal projects, org key for org projects).
#[wasm_bindgen]
pub fn name_decrypt_env(key: &[u8], env_id: &str, ciphertext: &[u8]) -> Result<Vec<u8>, JsError> {
    sotto_core::names::decrypt_env_name(&key32(key)?, env_id, ciphertext)
        .map_err(|e| JsError::new(&e.to_string()))
}
