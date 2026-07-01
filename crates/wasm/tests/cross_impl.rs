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
