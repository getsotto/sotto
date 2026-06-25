//! Human-facing key encoding: Crockford base32, versioned + checksummed. Used for the secret
//! key (`SK1-…`) and the Emergency Kit recovery key (`RK1-…`). Full encode/decode + checksum
//! lands in M1.

/// Crockford base32 alphabet — excludes the ambiguous letters I, L, O, and U.
pub const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_has_32_symbols_and_no_ambiguous_letters() {
        assert_eq!(CROCKFORD_ALPHABET.len(), 32);
        for ambiguous in [b'I', b'L', b'O', b'U'] {
            assert!(!CROCKFORD_ALPHABET.contains(&ambiguous));
        }
    }
}
