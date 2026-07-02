//! Cross-implementation gate: the WASM build must satisfy the same golden vectors as native,
//! proving native-produced ciphertext decrypts in WASM and the runtime (incl. the JS-backed
//! CSPRNG and wasm-bindgen layer) works.
//!
//! Run: `wasm-pack test --node crates/wasm`

#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

/// Runs the shared golden-vector checks (the same function native asserts) inside WASM.
#[wasm_bindgen_test]
fn golden_vectors_match_in_wasm() {
    sotto_core::vectors::verify_cross_impl().expect("WASM must satisfy the golden vectors");
}

/// Exercises the wasm-bindgen binding layer end-to-end.
#[wasm_bindgen_test]
fn aead_round_trip_via_bindings() {
    let key = [9u8; 32];
    let env = sotto_wasm::aead_seal(&key, b"hello", b"ctx").unwrap_or_else(|_| panic!("seal"));
    let pt = sotto_wasm::aead_open(&key, &env, b"ctx").unwrap_or_else(|_| panic!("open"));
    assert_eq!(pt, b"hello");
}

/// Share seal/open via the bindings (the recipient decrypt path runs in WASM).
#[wasm_bindgen_test]
fn share_round_trip_via_bindings() {
    let key = [7u8; 32];
    let env = sotto_wasm::share_seal(&key, b"share-me").unwrap_or_else(|_| panic!("seal"));
    let pt = sotto_wasm::share_open(&key, &env).unwrap_or_else(|_| panic!("open"));
    assert_eq!(pt, b"share-me");
}

/// Decoding a human key string (a pasted secret key) works in WASM.
#[wasm_bindgen_test]
fn decode_key_via_bindings() {
    // The golden `KEYSTRING` vector: `encode_key("SK", 1, &[0xAB; 16])`.
    let bytes = sotto_wasm::format_decode_key("SK", 1, "SK1-NENTQ-AXBNE-NTQAX-BNENT-QAXBN-DDBW")
        .unwrap_or_else(|_| panic!("decode"));
    assert_eq!(bytes, [0xAB; 16]);
}

/// Rotation rewrap via the bindings: the untouched ciphertext decrypts under the new key.
#[wasm_bindgen_test]
fn rewrap_via_bindings() {
    let old = [0x11u8; 32];
    let new = [0x22u8; 32];
    let enc = sotto_core::vault::encrypt_secret(&old, "e1", "s1", 3, b"NAME", b"value");
    let rewrapped = sotto_wasm::vault_rewrap_data_key(&old, &new, "e1", "s1", 3, &enc.enc_data_key)
        .unwrap_or_else(|_| panic!("rewrap"));
    let value = sotto_wasm::vault_decrypt_value(&new, "e1", "s1", 3, &enc.enc_value, &rewrapped)
        .unwrap_or_else(|_| panic!("decrypt under new key"));
    assert_eq!(value, b"value");
    // The old key no longer opens the rewrapped data key.
    assert!(
        sotto_wasm::vault_decrypt_value(&old, "e1", "s1", 3, &enc.enc_value, &rewrapped).is_err()
    );
}

/// Metadata name decryption via the bindings agrees with the native scheme.
#[wasm_bindgen_test]
fn name_decrypt_via_bindings() {
    let key = [0x44u8; 32];
    let enc = sotto_core::names::encrypt_project_name(&key, "p1", b"acme");
    assert_eq!(
        sotto_wasm::name_decrypt_project(&key, "p1", &enc).unwrap_or_else(|_| panic!("project")),
        b"acme"
    );
    let enc = sotto_core::names::encrypt_env_name(&key, "e1", b"prod");
    assert_eq!(
        sotto_wasm::name_decrypt_env(&key, "e1", &enc).unwrap_or_else(|_| panic!("env")),
        b"prod"
    );
    let enc = sotto_core::names::encrypt_org_name(&key, "o1", b"team");
    assert_eq!(
        sotto_wasm::name_decrypt_org(&key, "o1", &enc).unwrap_or_else(|_| panic!("org")),
        b"team"
    );
    // The wrong record id fails (AAD binding survives the boundary).
    assert!(sotto_wasm::name_decrypt_org(&key, "o2", &enc).is_err());
}

/// Vault key hierarchy via the bindings (the in-browser vault read/write path runs in WASM).
#[wasm_bindgen_test]
fn vault_round_trip_via_bindings() {
    let master = [0x55u8; 32];
    let vault_key = [0x66u8; 32];
    // The account keypair and its master-wrapped private key, as an account carries them.
    let keypair = sotto_core::wrap::keypair_from_secret(&[0x77u8; 32]);
    let enc_private_keys = sotto_core::vault::wrap_private_key(&master, &keypair.secret);

    // Grant the vault key to the member, then open it from the master + enc_private_keys.
    let grant = sotto_wasm::vault_grant_key(&keypair.public, &vault_key)
        .unwrap_or_else(|_| panic!("grant"));
    let opened = sotto_wasm::vault_open_grant(&master, &enc_private_keys, &grant)
        .unwrap_or_else(|_| panic!("open grant"));
    assert_eq!(opened, vault_key.to_vec());

    let enc = sotto_wasm::vault_encrypt_secret(&vault_key, "e1", "s1", 2, b"NAME", b"value")
        .unwrap_or_else(|_| panic!("encrypt"));
    let name = sotto_wasm::vault_decrypt_name(
        &vault_key,
        "e1",
        "s1",
        2,
        &enc.enc_name(),
        &enc.enc_data_key(),
    )
    .unwrap_or_else(|_| panic!("decrypt name"));
    let value = sotto_wasm::vault_decrypt_value(
        &vault_key,
        "e1",
        "s1",
        2,
        &enc.enc_value(),
        &enc.enc_data_key(),
    )
    .unwrap_or_else(|_| panic!("decrypt value"));
    assert_eq!(name, b"NAME");
    assert_eq!(value, b"value");
}
