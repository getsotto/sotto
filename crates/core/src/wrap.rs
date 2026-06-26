//! Key wrapping for the key hierarchy.
//!
//! - **Public-key wrapping** ([`seal_to_public`] / [`open_sealed`]) — X25519 sealed boxes:
//!   encrypt a key (e.g. an env vault key) to a recipient's public key so only they can
//!   recover it. Anonymous (ephemeral sender), exactly what sharing a vault key needs.
//! - **Symmetric wrapping** ([`wrap_key`] / [`unwrap_key`]) — wrap a key under another key
//!   (e.g. a per-secret data key under the vault key, or a user's private key under the master
//!   key) using the AEAD, binding a context as `aad`.

use dryoc::dryocbox::{DryocBox, VecBox};
use dryoc::keypair::{PublicKey, SecretKey, StackKeyPair};
use dryoc::types::Bytes;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::aead;
use crate::error::Error;

/// X25519 public key length, in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;
/// X25519 secret key length, in bytes.
pub const SECRET_KEY_LEN: usize = 32;

/// An X25519 keypair (raw bytes). The secret key is zeroized on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Keypair {
    /// Public key (safe to share / store in the clear).
    #[zeroize(skip)]
    pub public: [u8; PUBLIC_KEY_LEN],
    /// Secret key — keep private; zeroized on drop.
    pub secret: [u8; SECRET_KEY_LEN],
}

/// Generate a fresh X25519 keypair.
pub fn generate_keypair() -> Keypair {
    let kp = StackKeyPair::generate();
    // Copy straight into the returned `Keypair` so the only raw secret buffer here is the one
    // zeroized on drop — copying via a stack-local `[u8; 32]` (which is `Copy`) would leave a
    // stray, un-zeroized duplicate of the secret behind on the stack.
    let mut out = Keypair {
        public: [0u8; PUBLIC_KEY_LEN],
        secret: [0u8; SECRET_KEY_LEN],
    };
    out.public.copy_from_slice(kp.public_key.as_slice());
    out.secret.copy_from_slice(kp.secret_key.as_slice());
    out
}

/// Wrap `plaintext` (typically a symmetric key) to `recipient_public` via an X25519 sealed box.
/// Returns sealed bytes (`ephemeral_pk ‖ mac ‖ ciphertext`).
pub fn seal_to_public(
    recipient_public: &[u8; PUBLIC_KEY_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    let pk = PublicKey::from(*recipient_public);
    let boxed: VecBox = DryocBox::seal_to_vecbox(plaintext, &pk)?;
    Ok(boxed.to_vec())
}

/// Open a sealed box addressed to `keypair`, returning the wrapped plaintext.
pub fn open_sealed(keypair: &Keypair, sealed: &[u8]) -> Result<Vec<u8>, Error> {
    let kp = StackKeyPair::from_secret_key(SecretKey::from(keypair.secret));
    let boxed: VecBox = DryocBox::from_sealed_bytes(sealed)?;
    Ok(boxed.unseal_to_vec(&kp)?)
}

/// Symmetric key length, in bytes.
pub const KEY_LEN: usize = 32;

/// Wrap `key` under the key-encryption key `kek`, binding `aad`. Returns an AEAD envelope.
pub fn wrap_key(kek: &[u8; KEY_LEN], key: &[u8; KEY_LEN], aad: &[u8]) -> Vec<u8> {
    aead::seal(kek, key, aad)
}

/// Unwrap a key wrapped under `kek`, verifying `aad`.
pub fn unwrap_key(kek: &[u8; KEY_LEN], wrapped: &[u8], aad: &[u8]) -> Result<[u8; KEY_LEN], Error> {
    let mut pt = aead::open(kek, wrapped, aad)?;
    // Zeroize the decrypted plaintext on every path, including the wrong-length error — `pt`
    // holds secret material and a plain `Vec<u8>` is not zeroized on drop.
    let key: Result<[u8; KEY_LEN], Error> = pt
        .as_slice()
        .try_into()
        .map_err(|_| Error::Malformed("wrapped key wrong length"));
    pt.zeroize();
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random;

    #[test]
    fn generated_public_matches_secret() {
        let kp = generate_keypair();
        // Re-deriving the public key from the secret must agree.
        let kp2 = open_then_public(&kp.secret);
        assert_eq!(kp.public, kp2);
    }

    fn open_then_public(secret: &[u8; SECRET_KEY_LEN]) -> [u8; PUBLIC_KEY_LEN] {
        let kp = StackKeyPair::from_secret_key(SecretKey::from(*secret));
        let mut public = [0u8; PUBLIC_KEY_LEN];
        public.copy_from_slice(kp.public_key.as_slice());
        public
    }

    #[test]
    fn sealed_box_round_trip() {
        let member = generate_keypair();
        let secret = random::bytes::<32>();
        let sealed = seal_to_public(&member.public, &secret).unwrap();
        assert_eq!(open_sealed(&member, &sealed).unwrap(), secret);
    }

    #[test]
    fn wrong_recipient_cannot_unseal() {
        let member = generate_keypair();
        let intruder = generate_keypair();
        let sealed = seal_to_public(&member.public, b"vault-key").unwrap();
        assert!(open_sealed(&intruder, &sealed).is_err());
    }

    #[test]
    fn symmetric_wrap_round_trip() {
        let kek = random::bytes::<KEY_LEN>();
        let key = random::bytes::<KEY_LEN>();
        let wrapped = wrap_key(&kek, &key, b"datakey");
        assert_eq!(unwrap_key(&kek, &wrapped, b"datakey").unwrap(), key);
        assert!(unwrap_key(&kek, &wrapped, b"wrong-context").is_err());
    }

    /// End-to-end key hierarchy: a member receives an env vault key via a sealed box, uses it
    /// to unwrap a per-secret data key, and decrypts a secret bound to its identity.
    #[test]
    fn key_hierarchy_round_trip() {
        let member = generate_keypair();

        // Env vault key, shared to the member as a sealed-box grant.
        let vault_key = random::bytes::<KEY_LEN>();
        let grant = seal_to_public(&member.public, &vault_key).unwrap();
        let vault_key_recovered: [u8; KEY_LEN] = open_sealed(&member, &grant)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();
        assert_eq!(vault_key_recovered, vault_key);

        // Per-secret data key, wrapped under the vault key.
        let data_key = random::bytes::<KEY_LEN>();
        let wrapped_dk = wrap_key(&vault_key_recovered, &data_key, b"datakey");
        let data_key_recovered = unwrap_key(&vault_key_recovered, &wrapped_dk, b"datakey").unwrap();

        // The secret itself, encrypted under the data key with AAD context binding.
        let aad = b"env=prod|name=DATABASE_URL|v=1";
        let env = aead::seal(&data_key_recovered, b"postgres://prod", aad);
        assert_eq!(
            aead::open(&data_key_recovered, &env, aad).unwrap(),
            b"postgres://prod"
        );
    }
}
