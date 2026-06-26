//! Sotto crypto core — one audited implementation, shared by the CLI (native) and the web
//! client (WASM).
//!
//! Built on [`dryoc`](https://docs.rs/dryoc) (pure-Rust libsodium):
//! - [`aead`] — XChaCha20-Poly1305 (secretstream) with associated-data binding
//! - [`kdf`] — Argon2id + BLAKE2b combine → master key, and BLAKE2b domain-separated subkeys
//! - [`wrap`] — X25519 sealed-box key wrapping (public-key) and symmetric key wrapping
//! - [`format`] — Crockford base32, versioned + checksummed human key strings
//! - [`envelope`] — the versioned, self-describing ciphertext format
//!
//! Still to come: cross-implementation test vectors (native encrypts ↔ WASM decrypts).

pub mod aead;
pub mod envelope;
pub mod error;
pub mod format;
pub mod kdf;
pub mod random;
pub mod wrap;

pub use error::Error;

/// Current crypto scheme version. Surfaced so the WASM build and the native build can assert
/// they agree — the cross-implementation gate.
pub const SCHEME_VERSION: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_version_is_stable() {
        assert_eq!(SCHEME_VERSION, 1);
    }
}
