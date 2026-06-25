//! Sotto crypto core — one audited implementation, shared by the CLI (native) and the web
//! client (WASM). See `docs/CRYPTO.md`.
//!
//! M1 implements: KDF (Argon2id + HKDF combine), the versioned envelope, XChaCha20-Poly1305
//! with AAD context-binding, X25519 sealed-box key wrapping, and the Crockford key formats —
//! pinned by cross-implementation test vectors (native encrypts ↔ WASM decrypts, byte-for-byte).

pub mod envelope;
pub mod format;

/// Current crypto scheme version. Surfaced so the WASM build and the native build can assert
/// they agree — the M1 cross-implementation gate.
pub const SCHEME_VERSION: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_version_is_stable() {
        assert_eq!(SCHEME_VERSION, 1);
    }
}
