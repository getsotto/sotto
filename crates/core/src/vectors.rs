//! Golden cross-implementation test vectors.
//!
//! [`verify_cross_impl`] recomputes the crypto core's behaviour and checks it against pinned
//! golden values. The SAME function is run by a native unit test (below) and by the WASM
//! `wasm-bindgen-test` in `crates/wasm` — so the two builds are proven to agree.
//!
//! The crypto core is pure Rust (dryoc plus RustCrypto's `chacha20poly1305` for AEAD), so native
//! and WASM run identical code; the gate primarily proves the WASM build + runtime integration
//! (getrandom, wasm-bindgen, memory) works and can decrypt native-produced ciphertext. The
//! Argon2id master-key vector is checked natively only (it's identical in WASM by construction,
//! and avoids running 256 MiB Argon2 inside wasm).
//!
//! Regenerate with `cargo run -p sotto-core --example gen_vectors`.

use crate::{aead, format, kdf, share, wrap};

const SUBKEY_HEX: &str = "a9c378fc08287281b23194842d223fd52f349c259d1c756ce31953ba9a1eb051";
const CROCKFORD: &str = "000G40R40M30E209185GR38E1W";
const KEYSTRING: &str = "SK1-NENTQ-AXBNE-NTQAX-BNENT-QAXBN-DDBW";

const AEAD_KEY: [u8; 32] = [0x11; 32];
const AEAD_AAD: &[u8] = b"env=prod|name=DATABASE_URL|v=1";
const AEAD_PT: &[u8] = b"postgres://prod-db:5432/app";
const AEAD_ENV_HEX: &str = "010108afaabde244213e347b8b70d4fcfc922a0f695b3e4a7bd14c030f3cbb840c7c37bc19cd6b49c9e4758f10c256e2b5cbdd1e03ff2cbe2d766e60b00fa3cdfef025cf48";

const SB_SECRET_HEX: &str = "5755ecb1a6b3fa4e4bd3232678a440dcb3ca8ef1408e40fa8a5c346cf18c78ec";
const SB_PT: &[u8] = b"vault-key-material-here-32-bytes!";
const SB_SEALED_HEX: &str = "f97cfa5aa9fa49c213b6d308697a1fece85ab9d9663cc5303bcf75baf17f1358a103c315299a03918fbbdadec9516ae1afaaef59215a9950b9dfa89de041de637ca58e07426f06f69713532d9fcc4acccb";

const SHARE_KEY: [u8; 32] = [0x33; 32];
const SHARE_PT: &[u8] = b"share-secret-value";
const SHARE_ENV_HEX: &str = "01012d36369a6771d17a1499ea93651931075e9ab1606c38e20c0885df13bc7508aa4e6dadaf7173b4d4c03bffb2aa81b7eb51333ed415acee97f59d";

/// Decode a hex string to bytes. Panics on malformed input (vector helper).
fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

/// Recompute every vector and check it against the pinned golden value. Returns `Err(label)`
/// for the first mismatch. Shared by the native test and the WASM `wasm-bindgen-test`.
pub fn verify_cross_impl() -> Result<(), &'static str> {
    // Deterministic AEAD decryption of a native-produced envelope.
    let aead_env = unhex(AEAD_ENV_HEX);
    let pt = aead::open(&AEAD_KEY, &aead_env, AEAD_AAD).map_err(|_| "aead open")?;
    if pt.as_slice() != AEAD_PT {
        return Err("aead plaintext mismatch");
    }
    // AAD binding must be enforced.
    if aead::open(&AEAD_KEY, &aead_env, b"env=dev").is_ok() {
        return Err("aead aad not enforced");
    }

    // Sealed-box decryption of a native-produced blob.
    let secret: [u8; 32] = unhex(SB_SECRET_HEX)
        .as_slice()
        .try_into()
        .map_err(|_| "sb secret len")?;
    let kp = wrap::keypair_from_secret(&secret);
    let opened = wrap::open_sealed(&kp, &unhex(SB_SEALED_HEX)).map_err(|_| "sealed open")?;
    if opened.as_slice() != SB_PT {
        return Err("sealed plaintext mismatch");
    }

    // Share envelope: native-produced, opens in every build; bound to the share AAD.
    let share_env = unhex(SHARE_ENV_HEX);
    let sp = share::open(&SHARE_KEY, &share_env).map_err(|_| "share open")?;
    if sp.as_slice() != SHARE_PT {
        return Err("share plaintext mismatch");
    }
    if aead::open(&SHARE_KEY, &share_env, b"sotto/v1/other").is_ok() {
        return Err("share aad not enforced");
    }

    // Deterministic key derivation (BLAKE2b).
    let sub = kdf::derive_subkey(&[0x01; 32], b"vaultkey", 0).map_err(|_| "subkey")?;
    if sub.as_slice() != unhex(SUBKEY_HEX).as_slice() {
        return Err("subkey mismatch");
    }

    // Deterministic encodings.
    let data: Vec<u8> = (0u8..16).collect();
    if format::encode(&data).as_str() != CROCKFORD {
        return Err("crockford mismatch");
    }
    if format::encode_key("SK", 1, &[0xAB; 16]).as_str() != KEYSTRING {
        return Err("keystring mismatch");
    }

    // A live round-trip — proves the runtime, including the CSPRNG, works in this build.
    let expected: &[u8] = b"roundtrip";
    let env = aead::seal(&AEAD_KEY, expected, b"ctx");
    if aead::open(&AEAD_KEY, &env, b"ctx")
        .map_err(|_| "roundtrip open")?
        .as_slice()
        != expected
    {
        return Err("roundtrip mismatch");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER_PASSWORD: &[u8] = b"correct horse battery staple";
    const MASTER_SECRET_KEY: &[u8] = b"sotto-secret-key";
    const MASTER_SALT: [u8; kdf::SALT_LEN] = [0x42; kdf::SALT_LEN];
    const MASTER_KEY_HEX: &str = "5c7b2945673933b0047ce1b8401e839c4024c6b3bbe05977169fbbd722a622ce";

    #[test]
    fn cross_impl_vectors_match_native() {
        verify_cross_impl().expect("native must satisfy the golden vectors");
    }

    #[test]
    fn master_key_vector() {
        let mk = kdf::derive_master_key(MASTER_PASSWORD, MASTER_SECRET_KEY, &MASTER_SALT).unwrap();
        assert_eq!(mk.as_slice(), unhex(MASTER_KEY_HEX).as_slice());
    }
}
