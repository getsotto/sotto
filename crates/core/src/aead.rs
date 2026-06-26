//! Authenticated encryption with associated data (AEAD).
//!
//! Built on dryoc's XChaCha20-Poly1305 `secretstream`, used as a one-shot (a single message
//! sealed with `Tag::FINAL`). The `aad` is bound into the authentication tag, so a server that
//! moves or rolls back a blob causes decryption to fail closed (see the data model: bind
//! `aad = scheme ‖ env_id ‖ secret_id ‖ version ‖ field`).

use dryoc::dryocstream::{DryocStream, Header, Key, Tag};
use dryoc::types::Bytes;

use crate::envelope::{Alg, HEADER_LEN, SCHEME_V1};
use crate::error::Error;

/// Symmetric key length (XChaCha20-Poly1305), in bytes.
pub const KEY_LEN: usize = 32;

/// Encrypt `plaintext` under `key`, binding `aad`, returning a versioned envelope.
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let key = Key::from(*key);
    let (mut stream, header): (_, Header) = DryocStream::init_push(&key);
    // dryoc's `Bytes` bound requires a sized type, and message + AAD must share a type.
    let message = plaintext.to_vec();
    let aad = aad.to_vec();
    let ciphertext = stream
        .push_to_vec(&message, Some(&aad), Tag::FINAL)
        .expect("secretstream push cannot fail for valid inputs");

    let mut out = Vec::with_capacity(2 + HEADER_LEN + ciphertext.len());
    out.push(SCHEME_V1);
    out.push(Alg::XChaCha20Poly1305Stream as u8);
    out.extend_from_slice(header.as_slice());
    out.extend_from_slice(&ciphertext);
    out
}

/// Decrypt a versioned envelope under `key`, verifying `aad`.
///
/// Returns [`Error::Crypto`] on any authentication failure (wrong key, tampered ciphertext, or
/// mismatched `aad`) — deliberately without distinguishing the cause.
pub fn open(key: &[u8; KEY_LEN], envelope: &[u8], aad: &[u8]) -> Result<Vec<u8>, Error> {
    if envelope.len() < 2 + HEADER_LEN {
        return Err(Error::Malformed("envelope too short"));
    }
    let scheme = envelope[0];
    let alg = envelope[1];
    if scheme != SCHEME_V1 || Alg::from_u8(alg) != Some(Alg::XChaCha20Poly1305Stream) {
        return Err(Error::UnsupportedScheme { scheme, alg });
    }

    let header = Header::try_from(&envelope[2..2 + HEADER_LEN])
        .map_err(|_| Error::Malformed("bad header"))?;
    let ciphertext = envelope[2 + HEADER_LEN..].to_vec();
    let aad = aad.to_vec();

    let key = Key::from(*key);
    let mut stream = DryocStream::init_pull(&key, &header);
    let (plaintext, tag) = stream.pull_to_vec(&ciphertext, Some(&aad))?;
    if tag != Tag::FINAL {
        return Err(Error::Malformed("unexpected stream tag"));
    }
    Ok(plaintext)
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
