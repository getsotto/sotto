//! Versioned, self-describing ciphertext envelope. See `docs/CRYPTO.md` §5.
//!
//! Wire layout (implemented in M1):
//! `[ scheme: u8 ][ alg: u8 ][ nonce: 24 ][ ciphertext ‖ Poly1305 tag ]`
//!
//! The envelope is fixed-layout per scheme (no algorithm negotiation — avoids downgrade
//! attacks). New schemes append; ids are never reused.

/// Algorithms in the scheme registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Alg {
    /// XChaCha20-Poly1305 with a 192-bit random nonce (scheme 1).
    XChaCha20Poly1305 = 1,
}
