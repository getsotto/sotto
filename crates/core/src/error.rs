//! Error type for the crypto core.

use thiserror::Error;

/// Errors returned by the crypto core.
#[derive(Debug, Error)]
pub enum Error {
    /// A ciphertext, envelope, or key string was structurally invalid.
    #[error("malformed data: {0}")]
    Malformed(&'static str),

    /// The envelope scheme/algorithm byte is not supported by this build.
    #[error("unsupported scheme {scheme} / alg {alg}")]
    UnsupportedScheme { scheme: u8, alg: u8 },

    /// A key string failed its checksum (likely a transcription typo).
    #[error("key checksum mismatch")]
    Checksum,

    /// A key string had an unexpected prefix or version.
    #[error("unexpected key prefix or version")]
    KeyPrefix,

    /// An underlying cryptographic operation failed (e.g. authentication).
    ///
    /// Intentionally opaque — we never leak why an authenticated decryption failed.
    #[error("cryptographic operation failed")]
    Crypto,
}

impl From<dryoc::Error> for Error {
    fn from(_: dryoc::Error) -> Self {
        Error::Crypto
    }
}
