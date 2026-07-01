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

/// Vault key hierarchy via the bindings (the in-browser vault read/write path runs in WASM).
#[wasm_bindgen_test]
fn vault_round_trip_via_bindings() {
    let master = [0x55u8; 32];
    let vault_key = [0x66u8; 32];

    let enc_vk =
        sotto_wasm::vault_wrap_key(&master, &vault_key, "e1").unwrap_or_else(|_| panic!("wrap"));
    let unwrapped =
        sotto_wasm::vault_unwrap_key(&master, &enc_vk, "e1").unwrap_or_else(|_| panic!("unwrap"));
    assert_eq!(unwrapped, vault_key);

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
