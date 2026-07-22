//! Versioned, self-describing ciphertext envelope.
//!
//! Wire layout:
//! `[ scheme: u8 ][ alg: u8 ][ nonce: 24 ][ ciphertext ‖ Poly1305 tag ]`
//!
//! Fixed layout per scheme (no algorithm negotiation - avoids downgrade attacks). New schemes
//! append; algorithm ids are never reused.

/// Current envelope scheme.
pub const SCHEME_V1: u8 = 1;

/// XChaCha20-Poly1305 nonce length, in bytes (192-bit; random nonces are collision-safe).
pub const NONCE_LEN: usize = 24;

/// Algorithms in the scheme registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Alg {
    /// XChaCha20-Poly1305 one-shot AEAD with a 192-bit random nonce (scheme 1).
    XChaCha20Poly1305 = 1,
}

impl Alg {
    /// Parse an algorithm id from its on-the-wire byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Alg::XChaCha20Poly1305),
            _ => None,
        }
    }
}
