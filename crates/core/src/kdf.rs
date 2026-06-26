//! Key derivation.
//!
//! - master key = keyed-BLAKE2b(key = Argon2id(password, salt), msg = secret_key)
//! - subkeys via BLAKE2b domain separation (libsodium `crypto_kdf`)
//!
//! The high-entropy secret key never reaches the server and isn't derivable from server-stored
//! data, so it is the dominant security margin; Argon2id hardens the stolen-device case.

use dryoc::classic::crypto_kdf::crypto_kdf_derive_from_key;
use dryoc::classic::crypto_pwhash::{crypto_pwhash, PasswordHashAlgorithm};
use dryoc::generichash::{GenericHash, Key as GhKey};
use zeroize::Zeroize;

use crate::error::Error;

/// Master / vault / subkey length, in bytes.
pub const KEY_LEN: usize = 32;
/// Argon2id salt length, in bytes.
pub const SALT_LEN: usize = 16;
/// BLAKE2b key-derivation context length, in bytes.
pub const CONTEXT_LEN: usize = 8;

/// Argon2id iterations (crypto spec: t = 4).
pub const OPSLIMIT: u64 = 4;
/// Argon2id memory limit, in bytes (256 MiB).
pub const MEMLIMIT: usize = 256 * 1024 * 1024;

/// Derive the master key from the master password, the high-entropy secret key, and a salt.
pub fn derive_master_key(
    password: &[u8],
    secret_key: &[u8],
    salt: &[u8; SALT_LEN],
) -> Result<[u8; KEY_LEN], Error> {
    let mut a = [0u8; KEY_LEN];
    crypto_pwhash(
        &mut a,
        password,
        salt,
        OPSLIMIT,
        MEMLIMIT,
        PasswordHashAlgorithm::Argon2id13,
    )?;

    let gh_key = GhKey::try_from(&a[..]).map_err(|_| Error::Crypto)?;
    let out = GenericHash::hash_with_defaults_to_vec::<_, GhKey>(secret_key, Some(&gh_key))
        .map_err(|_| Error::Crypto)?;
    a.zeroize();

    if out.len() != KEY_LEN {
        return Err(Error::Crypto);
    }
    let mut master = [0u8; KEY_LEN];
    master.copy_from_slice(&out);
    Ok(master)
}

/// Derive a domain-separated subkey from a master key.
///
/// `context` is an 8-byte label (e.g. `b"vaultkey"`); `subkey_id` distinguishes subkeys that
/// share a context. Backed by BLAKE2b (libsodium `crypto_kdf`).
pub fn derive_subkey(
    master: &[u8; KEY_LEN],
    context: &[u8; CONTEXT_LEN],
    subkey_id: u64,
) -> Result<[u8; KEY_LEN], Error> {
    let mut sub = [0u8; KEY_LEN];
    crypto_kdf_derive_from_key(&mut sub, subkey_id, context, master)?;
    Ok(sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_key_is_deterministic() {
        let salt = [7u8; SALT_LEN];
        let k1 = derive_master_key(b"correct horse", b"secret-key", &salt).unwrap();
        let k2 = derive_master_key(b"correct horse", b"secret-key", &salt).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn master_key_varies_with_each_input() {
        let salt = [7u8; SALT_LEN];
        let base = derive_master_key(b"pw", b"sk", &salt).unwrap();
        assert_ne!(base, derive_master_key(b"PW", b"sk", &salt).unwrap());
        assert_ne!(base, derive_master_key(b"pw", b"SK", &salt).unwrap());
        assert_ne!(
            base,
            derive_master_key(b"pw", b"sk", &[8u8; SALT_LEN]).unwrap()
        );
    }

    #[test]
    fn subkeys_are_deterministic_and_separated() {
        let master = [1u8; KEY_LEN];
        let a0 = derive_subkey(&master, b"vaultkey", 0).unwrap();
        assert_eq!(a0, derive_subkey(&master, b"vaultkey", 0).unwrap());
        assert_ne!(a0, derive_subkey(&master, b"vaultkey", 1).unwrap());
        assert_ne!(a0, derive_subkey(&master, b"signkey_", 0).unwrap());
    }
}
