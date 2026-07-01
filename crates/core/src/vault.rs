//! The environment key hierarchy and secret encryption — one audited implementation shared by the
//! CLI (native) and the web client (WASM).
//!
//! A per-environment **vault key** is shared to each member as a **grant**: it is sealed (X25519
//! anonymous box) to the member's public key. A member opens a grant with their account keypair
//! (itself sealed under the master key). Each secret write generates a fresh per-secret **data
//! key** wrapped under the vault key; the name and value are sealed under the data key, with AAD
//! binding their location (`env`, `secret`, `version`, `field`) so blobs can't be swapped,
//! relocated, or mixed across secrets, environments, or versions. Grants themselves carry no AAD
//! (sealed boxes are anonymous); a grant substituted onto the wrong environment fails closed
//! because that environment's data keys are wrapped under a different vault key. (AAD binding does
//! not detect a rollback of a whole row to an earlier consistent version — that needs separate
//! freshness / monotonic-version tracking, handled at the sync layer.)

use zeroize::Zeroize;

use crate::error::Error;
use crate::{aead, random, wrap};

/// Symmetric key length shared across the hierarchy.
pub const KEY_LEN: usize = 32;

/// AAD binding the account's X25519 private key wrapped under the master key.
const PRIVKEYS_AAD: &[u8] = b"sotto/v1/privkeys";

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

/// Wrap the account's X25519 private key under the master key (stored as `enc_private_keys`).
pub fn wrap_private_key(master_key: &[u8; KEY_LEN], x25519_secret: &[u8; KEY_LEN]) -> Vec<u8> {
    wrap::wrap_key(master_key, x25519_secret, PRIVKEYS_AAD)
}

/// Recover the account keypair from `enc_private_keys` under the master key.
pub fn open_account_keypair(
    master_key: &[u8; KEY_LEN],
    enc_private_keys: &[u8],
) -> Result<wrap::Keypair, Error> {
    let mut secret = wrap::unwrap_key(master_key, enc_private_keys, PRIVKEYS_AAD)?;
    let keypair = wrap::keypair_from_secret(&secret);
    secret.zeroize();
    Ok(keypair)
}

/// Seal a vault key to a grantee's public key — a **grant**. The recipient opens it with their
/// account keypair. (Sealed boxes are anonymous, so the sender is not revealed and there is no AAD.)
pub fn grant_vault_key(
    recipient_public: &[u8; wrap::PUBLIC_KEY_LEN],
    vault_key: &[u8; KEY_LEN],
) -> Result<Vec<u8>, Error> {
    wrap::seal_to_public(recipient_public, vault_key)
}

/// Open a granted vault key with the grantee's keypair.
pub fn open_vault_key(keypair: &wrap::Keypair, grant: &[u8]) -> Result<[u8; KEY_LEN], Error> {
    let mut opened = wrap::open_sealed(keypair, grant)?;
    let key: Result<[u8; KEY_LEN], Error> = opened
        .as_slice()
        .try_into()
        .map_err(|_| Error::Malformed("granted vault key has the wrong length"));
    opened.zeroize();
    key
}

/// Recover the account keypair from `enc_private_keys`, then open a vault-key grant — the whole
/// path from master key + stored key material to a usable vault key (used by the web binding).
pub fn open_vault_grant(
    master_key: &[u8; KEY_LEN],
    enc_private_keys: &[u8],
    grant: &[u8],
) -> Result<[u8; KEY_LEN], Error> {
    let keypair = open_account_keypair(master_key, enc_private_keys)?;
    open_vault_key(&keypair, grant)
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
    fn vault_key_grant_round_trip() {
        let member = wrap::generate_keypair();
        let vault_key = generate_vault_key();
        let grant = grant_vault_key(&member.public, &vault_key).unwrap();

        assert_eq!(open_vault_key(&member, &grant).unwrap(), vault_key);
        // A different keypair can't open the grant.
        assert!(open_vault_key(&wrap::generate_keypair(), &grant).is_err());
    }

    #[test]
    fn account_keypair_and_grant_via_master() {
        let master = [0x42u8; KEY_LEN];
        let keypair = wrap::generate_keypair();
        // enc_private_keys, as `session::init` stores it.
        let enc_private_keys = wrap_private_key(&master, &keypair.secret);

        // Recovering the keypair reproduces the same public key.
        let recovered = open_account_keypair(&master, &enc_private_keys).unwrap();
        assert_eq!(recovered.public, keypair.public);

        // The whole path: seal a grant to the public key, open it from master + enc_private_keys.
        let vault_key = generate_vault_key();
        let grant = grant_vault_key(&keypair.public, &vault_key).unwrap();
        assert_eq!(
            open_vault_grant(&master, &enc_private_keys, &grant).unwrap(),
            vault_key
        );
        // A wrong master key can't recover the keypair, so the grant can't be opened.
        assert!(open_vault_grant(&[0x99; KEY_LEN], &enc_private_keys, &grant).is_err());
    }
}
