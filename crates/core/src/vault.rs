//! The environment key hierarchy and secret encryption — one audited implementation shared by the
//! CLI (native) and the web client (WASM).
//!
//! A per-environment **vault key** is wrapped under the master key. Each secret write generates a
//! fresh per-secret **data key** wrapped under the vault key; the name and value are sealed under
//! the data key, with AAD binding their location (`env`, `secret`, `version`, `field`) so blobs
//! can't be swapped, relocated, or mixed across secrets, environments, or versions. (AAD binding
//! alone does not detect a rollback of a whole row to an earlier consistent version — that needs
//! separate freshness / monotonic-version tracking, handled at the sync layer.)

use zeroize::Zeroize;

use crate::error::Error;
use crate::{aead, random, wrap};

/// Symmetric key length shared across the hierarchy.
pub const KEY_LEN: usize = 32;

/// Ciphertext components produced by encrypting one secret.
pub struct EncryptedSecret {
    pub enc_name: Vec<u8>,
    pub enc_value: Vec<u8>,
    pub enc_data_key: Vec<u8>,
}

/// Generate a fresh environment vault key.
pub fn generate_vault_key() -> [u8; KEY_LEN] {
    random::bytes()
}

/// Wrap a vault key under the master key for storage (used when creating an environment).
pub fn wrap_vault_key(
    master_key: &[u8; KEY_LEN],
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
) -> Vec<u8> {
    wrap::wrap_key(master_key, vault_key, vault_key_aad(env_id).as_bytes())
}

/// Unwrap the environment vault key under the master key. Doubles as the unlock check: a wrong
/// master key (or tampered ciphertext) yields an error.
pub fn unwrap_vault_key(
    master_key: &[u8; KEY_LEN],
    enc_vault_key: &[u8],
    env_id: &str,
) -> Result<[u8; KEY_LEN], Error> {
    wrap::unwrap_key(master_key, enc_vault_key, vault_key_aad(env_id).as_bytes())
}

/// Encrypt a secret's name + value under a fresh data key wrapped by the vault key.
pub fn encrypt_secret(
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
    secret_id: &str,
    version: i64,
    name: &[u8],
    value: &[u8],
) -> EncryptedSecret {
    let mut data_key = random::bytes::<KEY_LEN>();
    let enc_name = aead::seal(
        &data_key,
        name,
        name_aad(env_id, secret_id, version).as_bytes(),
    );
    let enc_value = aead::seal(
        &data_key,
        value,
        value_aad(env_id, secret_id, version).as_bytes(),
    );
    let enc_data_key = wrap::wrap_key(
        vault_key,
        &data_key,
        data_key_aad(env_id, secret_id, version).as_bytes(),
    );
    data_key.zeroize();
    EncryptedSecret {
        enc_name,
        enc_value,
        enc_data_key,
    }
}

/// Decrypt a secret's name.
pub fn decrypt_name(
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
    secret_id: &str,
    version: i64,
    enc_name: &[u8],
    enc_data_key: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut data_key = unwrap_data_key(vault_key, env_id, secret_id, version, enc_data_key)?;
    let out = aead::open(
        &data_key,
        enc_name,
        name_aad(env_id, secret_id, version).as_bytes(),
    );
    data_key.zeroize();
    out
}

/// Decrypt a secret's value.
pub fn decrypt_value(
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
    secret_id: &str,
    version: i64,
    enc_value: &[u8],
    enc_data_key: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut data_key = unwrap_data_key(vault_key, env_id, secret_id, version, enc_data_key)?;
    let out = aead::open(
        &data_key,
        enc_value,
        value_aad(env_id, secret_id, version).as_bytes(),
    );
    data_key.zeroize();
    out
}

/// Decrypt both name and value, unwrapping the data key a single time.
pub fn decrypt_secret(
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
    secret_id: &str,
    version: i64,
    enc_name: &[u8],
    enc_value: &[u8],
    enc_data_key: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut data_key = unwrap_data_key(vault_key, env_id, secret_id, version, enc_data_key)?;
    let name = aead::open(
        &data_key,
        enc_name,
        name_aad(env_id, secret_id, version).as_bytes(),
    );
    let value = aead::open(
        &data_key,
        enc_value,
        value_aad(env_id, secret_id, version).as_bytes(),
    );
    data_key.zeroize();
    // If only one field authenticates, don't leave the other's plaintext lingering un-zeroized on
    // the error path; scrub it and surface the real failure.
    match (name, value) {
        (Ok(name), Ok(value)) => Ok((name, value)),
        (Ok(mut name), Err(e)) => {
            name.zeroize();
            Err(e)
        }
        (Err(e), Ok(mut value)) => {
            value.zeroize();
            Err(e)
        }
        (Err(e), Err(_)) => Err(e),
    }
}

fn unwrap_data_key(
    vault_key: &[u8; KEY_LEN],
    env_id: &str,
    secret_id: &str,
    version: i64,
    enc_data_key: &[u8],
) -> Result<[u8; KEY_LEN], Error> {
    wrap::unwrap_key(
        vault_key,
        enc_data_key,
        data_key_aad(env_id, secret_id, version).as_bytes(),
    )
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

    #[test]
    fn secret_round_trip_and_aad_binding() {
        let vault_key = generate_vault_key();
        let enc = encrypt_secret(&vault_key, "e1", "s1", 1, b"NAME", b"value");

        let (name, value) = decrypt_secret(
            &vault_key,
            "e1",
            "s1",
            1,
            &enc.enc_name,
            &enc.enc_value,
            &enc.enc_data_key,
        )
        .unwrap();
        assert_eq!(name, b"NAME");
        assert_eq!(value, b"value");

        // AAD binds env/secret/version: any mismatch fails.
        assert!(
            decrypt_value(&vault_key, "e1", "s1", 2, &enc.enc_value, &enc.enc_data_key).is_err()
        );
        assert!(
            decrypt_value(&vault_key, "e2", "s1", 1, &enc.enc_value, &enc.enc_data_key).is_err()
        );
        assert!(
            decrypt_value(&vault_key, "e1", "s2", 1, &enc.enc_value, &enc.enc_data_key).is_err()
        );
    }

    #[test]
    fn vault_key_wrap_round_trip() {
        let master = [0x42u8; KEY_LEN];
        let vault_key = generate_vault_key();
        let enc = wrap_vault_key(&master, &vault_key, "e1");
        assert_eq!(unwrap_vault_key(&master, &enc, "e1").unwrap(), vault_key);
        assert!(unwrap_vault_key(&[0x99; KEY_LEN], &enc, "e1").is_err());
    }
}
