//! Share-link crypto: sealing a single secret for a one-time / expiring link.
//!
//! A share is a secret sealed under a random 256-bit key that travels in the URL **fragment** and
//! never reaches the server. An optional passphrase adds a second factor: the AEAD key becomes the
//! combination of the fragment key with Argon2id(passphrase, salt) - the same construction as the
//! master key - so neither the link alone (fragment key, no passphrase) nor the server (salt +
//! ciphertext, no fragment key) can decrypt.

use crate::error::Error;
use crate::{aead, kdf};

/// Share fragment-key length, in bytes.
pub const KEY_LEN: usize = 32;

/// AAD binding a share envelope (domain separation from vault secrets).
const SHARE_AAD: &[u8] = b"sotto/v1/share";

/// Seal `plaintext` under the share AEAD key; returns a versioned envelope.
pub fn seal(aead_key: &[u8; KEY_LEN], plaintext: &[u8]) -> Vec<u8> {
    aead::seal(aead_key, plaintext, SHARE_AAD)
}

/// Open a share envelope under the share AEAD key.
pub fn open(aead_key: &[u8; KEY_LEN], envelope: &[u8]) -> Result<Vec<u8>, Error> {
    aead::open(aead_key, envelope, SHARE_AAD)
}

/// Derive the AEAD key for a passphrase-protected share: combine the fragment key with the
/// passphrase via Argon2id + keyed BLAKE2b.
pub fn passphrase_key(
    fragment_key: &[u8; KEY_LEN],
    passphrase: &[u8],
    salt: &[u8; kdf::SALT_LEN],
) -> Result<[u8; KEY_LEN], Error> {
    kdf::derive_master_key(passphrase, fragment_key, salt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random;

    #[test]
    fn seal_open_round_trip() {
        let key = random::bytes::<KEY_LEN>();
        let env = seal(&key, b"api-token-xyz");
        assert_eq!(open(&key, &env).unwrap(), b"api-token-xyz");
        assert!(open(&random::bytes::<KEY_LEN>(), &env).is_err());
    }

    #[test]
    fn passphrase_key_binds_passphrase_and_fragment() {
        let fragment = random::bytes::<KEY_LEN>();
        let salt = random::bytes::<{ kdf::SALT_LEN }>();
        let key = passphrase_key(&fragment, b"hunter2", &salt).unwrap();

        assert_eq!(key, passphrase_key(&fragment, b"hunter2", &salt).unwrap());
        assert_ne!(key, passphrase_key(&fragment, b"wrong", &salt).unwrap());
        assert_ne!(
            key,
            passphrase_key(&random::bytes::<KEY_LEN>(), b"hunter2", &salt).unwrap()
        );

        let env = seal(&key, b"secret");
        assert_eq!(open(&key, &env).unwrap(), b"secret");
    }
}
