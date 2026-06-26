//! Property tests: invariants the crypto core guarantees, over randomized inputs.
//!
//! Argon2id master-key derivation is intentionally excluded — at 256 MiB it's ~seconds per
//! call, so hundreds of cases would take minutes. It's deterministic and covered by a
//! known-answer test in `vectors.rs`.

use proptest::prelude::*;
use sotto_core::{aead, format, kdf, wrap};

fn key() -> impl Strategy<Value = [u8; 32]> {
    prop::array::uniform32(any::<u8>())
}

fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..max)
}

proptest! {
    /// AEAD: decrypting what we encrypted (with the same aad) returns the plaintext.
    #[test]
    fn aead_round_trip(k in key(), pt in bytes(512), aad in bytes(128)) {
        let env = aead::seal(&k, &pt, &aad);
        prop_assert_eq!(aead::open(&k, &env, &aad).expect("open"), pt);
    }

    /// AEAD: a different aad must fail (context binding).
    #[test]
    fn aead_wrong_aad_fails(k in key(), pt in bytes(256), aad1 in bytes(64), aad2 in bytes(64)) {
        prop_assume!(aad1 != aad2);
        let env = aead::seal(&k, &pt, &aad1);
        prop_assert!(aead::open(&k, &env, &aad2).is_err());
    }

    /// AEAD: a different key must fail (confidentiality).
    #[test]
    fn aead_wrong_key_fails(k1 in key(), k2 in key(), pt in bytes(256), aad in bytes(64)) {
        prop_assume!(k1 != k2);
        let env = aead::seal(&k1, &pt, &aad);
        prop_assert!(aead::open(&k2, &env, &aad).is_err());
    }

    /// AEAD: flipping any single byte of the envelope breaks decryption (integrity).
    #[test]
    fn aead_tamper_fails(k in key(), pt in bytes(256), aad in bytes(64), idx_seed in any::<usize>(), mask in 1u8..=255) {
        let mut env = aead::seal(&k, &pt, &aad);
        let idx = idx_seed % env.len();
        env[idx] ^= mask;
        prop_assert!(aead::open(&k, &env, &aad).is_err());
    }

    /// Crockford base32 round-trips over arbitrary byte lengths (bit-packing correctness).
    #[test]
    fn crockford_round_trip(data in bytes(256)) {
        prop_assert_eq!(format::decode(&format::encode(&data)).expect("decode"), data);
    }

    /// Versioned, checksummed key strings round-trip for any prefix/version/payload.
    #[test]
    fn key_string_round_trip(payload in prop::collection::vec(any::<u8>(), 1..64), version in any::<u8>()) {
        let s = format::encode_key("SK", version, &payload);
        prop_assert_eq!(format::decode_key("SK", version, &s).expect("decode_key"), payload);
    }

    /// Symmetric key wrapping round-trips.
    #[test]
    fn wrap_round_trip(kek in key(), k in key(), aad in bytes(64)) {
        let wrapped = wrap::wrap_key(&kek, &k, &aad);
        prop_assert_eq!(wrap::unwrap_key(&kek, &wrapped, &aad).expect("unwrap"), k);
    }

    /// X25519 sealed-box wrapping round-trips for the addressed keypair.
    #[test]
    fn sealed_box_round_trip(pt in bytes(256)) {
        let kp = wrap::generate_keypair();
        let sealed = wrap::seal_to_public(&kp.public, &pt).expect("seal");
        prop_assert_eq!(wrap::open_sealed(&kp, &sealed).expect("unseal"), pt);
    }

    /// Distinct subkey ids yield distinct subkeys (domain separation).
    #[test]
    fn subkeys_separated(master in key(), id1 in any::<u64>(), id2 in any::<u64>()) {
        prop_assume!(id1 != id2);
        let a = kdf::derive_subkey(&master, b"vaultkey", id1).expect("subkey");
        let b = kdf::derive_subkey(&master, b"vaultkey", id2).expect("subkey");
        prop_assert_ne!(a, b);
    }
}
