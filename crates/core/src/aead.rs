//! Authenticated encryption with associated data (AEAD).
//!
//! XChaCha20-Poly1305 (RustCrypto), one-shot, with a fresh random 192-bit nonce per call. The
//! `aad` is bound into the authentication tag, so a server that moves or rolls back a blob
//! causes decryption to fail closed (bind `aad = scheme ‖ env_id ‖ secret_id ‖ version ‖
//! field`).
//!
//! Why RustCrypto here and dryoc elsewhere: dryoc's secretstream uses `usize::to_le_bytes()`
//! for its length fields, which is 4 bytes on wasm32 and panics - broken in the browser. This
//! is the one place we step outside dryoc, for a wasm32-safe, textbook one-shot AEAD.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::envelope::{Alg, NONCE_LEN, SCHEME_V1};
use crate::error::Error;
use crate::random;

/// Symmetric key length (XChaCha20-Poly1305), in bytes.
pub const KEY_LEN: usize = 32;

/// Encrypt `plaintext` under `key`, binding `aad`, returning a versioned envelope.
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce_bytes = random::bytes::<NONCE_LEN>();
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("XChaCha20-Poly1305 encryption cannot fail for valid inputs");

    let mut out = Vec::with_capacity(2 + NONCE_LEN + ciphertext.len());
    out.push(SCHEME_V1);
    out.push(Alg::XChaCha20Poly1305 as u8);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

/// Decrypt a versioned envelope under `key`, verifying `aad`.
///
/// Returns [`Error::Crypto`] on any authentication failure (wrong key, tampered ciphertext, or
/// mismatched `aad`) - deliberately without distinguishing the cause.
pub fn open(key: &[u8; KEY_LEN], envelope: &[u8], aad: &[u8]) -> Result<Vec<u8>, Error> {
    if envelope.len() < 2 + NONCE_LEN {
        return Err(Error::Malformed("envelope too short"));
    }
    let scheme = envelope[0];
    let alg = envelope[1];
    if scheme != SCHEME_V1 || Alg::from_u8(alg) != Some(Alg::XChaCha20Poly1305) {
        return Err(Error::UnsupportedScheme { scheme, alg });
    }

    let nonce = XNonce::from_slice(&envelope[2..2 + NONCE_LEN]);
    let ciphertext = &envelope[2 + NONCE_LEN..];
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::Crypto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; KEY_LEN] {
        crate::random::bytes::<KEY_LEN>()
    }

    #[test]
    fn round_trip() {
        let k = key();
        let env = seal(&k, b"hello secrets", b"aad-context");
        let pt = open(&k, &env, b"aad-context").expect("decrypt");
        assert_eq!(pt, b"hello secrets");
    }

    #[test]
    fn wrong_aad_fails() {
        let k = key();
        let env = seal(&k, b"hello", b"env=prod");
        assert!(open(&k, &env, b"env=dev").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let k = key();
        let mut env = seal(&k, b"hello", b"aad");
        let last = env.len() - 1;
        env[last] ^= 0x01;
        assert!(open(&k, &env, b"aad").is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let env = seal(&key(), b"hello", b"aad");
        assert!(open(&key(), &env, b"aad").is_err());
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let k = key();
        let env = seal(&k, b"", b"");
        assert_eq!(open(&k, &env, b"").expect("decrypt"), b"");
    }
}
