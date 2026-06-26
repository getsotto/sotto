//! Versioned, self-describing ciphertext envelope.
//!
//! Wire layout:
//! `[ scheme: u8 ][ alg: u8 ][ secretstream header: 24 ][ ciphertext ‖ Poly1305 tag ]`
//!
//! Fixed layout per scheme (no algorithm negotiation — avoids downgrade attacks). New schemes
//! append; algorithm ids are never reused.

/// Current envelope scheme.
pub const SCHEME_V1: u8 = 1;

/// secretstream (XChaCha20-Poly1305) header length, in bytes.
pub const HEADER_LEN: usize = 24;

/// Algorithms in the scheme registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Alg {
    /// XChaCha20-Poly1305 secretstream, single message sealed with `Tag::FINAL` (scheme 1).
    XChaCha20Poly1305Stream = 1,
}

impl Alg {
    /// Parse an algorithm id from its on-the-wire byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Alg::XChaCha20Poly1305Stream),
            _ => None,
        }
    }
}
