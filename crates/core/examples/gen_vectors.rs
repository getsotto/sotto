//! Regenerates the golden cross-implementation test vectors in `src/vectors.rs`.
//!
//! Run: `cargo run -p sotto-core --example gen_vectors`
//!
//! Most vectors are deterministic; the AEAD envelope and sealed box embed a random
//! header/ephemeral key, so we capture one instance and pin it (decryption is deterministic).

use sotto_core::{aead, format, kdf, wrap};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let salt = [0x42u8; kdf::SALT_LEN];
    let mk = kdf::derive_master_key(b"correct horse battery staple", b"sotto-secret-key", &salt)
        .unwrap();
    println!("MASTER_KEY={}", hex(&mk));

    let sub = kdf::derive_subkey(&[0x01u8; 32], b"vaultkey", 0).unwrap();
    println!("SUBKEY={}", hex(&sub));

    let data: Vec<u8> = (0u8..16).collect();
    println!("CROCKFORD={}", format::encode(&data));
    println!("KEYSTRING={}", format::encode_key("SK", 1, &[0xABu8; 16]));

    let aead_key = [0x11u8; 32];
    let aad = b"env=prod|name=DATABASE_URL|v=1";
    let pt = b"postgres://prod-db:5432/app";
    let env = aead::seal(&aead_key, pt, aad);
    println!("AEAD_KEY={}", hex(&aead_key));
    println!("AEAD_AAD={}", hex(aad));
    println!("AEAD_PT={}", hex(pt));
    println!("AEAD_ENV={}", hex(&env));

    let kp = wrap::generate_keypair();
    let sb_pt = b"vault-key-material-here-32-bytes!";
    let sealed = wrap::seal_to_public(&kp.public, sb_pt).unwrap();
    println!("SB_SECRET={}", hex(&kp.secret));
    println!("SB_PT={}", hex(sb_pt));
    println!("SB_SEALED={}", hex(&sealed));
}
