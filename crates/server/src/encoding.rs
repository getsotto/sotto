//! Base64 transport helpers for opaque ciphertext fields.
//!
//! Ciphertext travels as base64 in JSON. Decoding rejects oversize input *before* allocating, so a
//! large field can't force a big allocation only to be rejected afterward.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use crate::error::{Error, Result};

/// Encode bytes as standard (padded) base64.
pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode a base64 field, rejecting malformed input or anything that would exceed `max` bytes.
pub fn decode(value: &str, field: &str, max: usize) -> Result<Vec<u8>> {
    // base64 is 4 characters per 3 bytes; bound the encoded length before decoding.
    let max_encoded = max.div_ceil(3) * 4;
    if value.len() > max_encoded {
        return Err(Error::BadRequest(format!("{field} exceeds {max} bytes")));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| Error::BadRequest(format!("{field} is not valid base64")))?;
    if bytes.len() > max {
        return Err(Error::BadRequest(format!("{field} exceeds {max} bytes")));
    }
    Ok(bytes)
}
