//! WASM bindings to the Sotto crypto core for the web client.
//!
//! The browser runs the SAME crypto core as the CLI, compiled to WASM — one implementation,
//! one audit surface. The cross-implementation gate (`tests/cross_impl.rs`) proves this build
//! decrypts native-produced ciphertext. More bindings (wrap, kdf, keygen) land with the web
//! client in M4.

use wasm_bindgen::prelude::*;

/// The crypto scheme version baked into this build. The gate asserts it equals the native
/// core's `SCHEME_VERSION`, so the two builds can never silently diverge.
#[wasm_bindgen]
pub fn scheme_version() -> u8 {
    sotto_core::SCHEME_VERSION
}

fn key32(key: &[u8]) -> Result<[u8; 32], JsError> {
    key.try_into()
        .map_err(|_| JsError::new("key must be 32 bytes"))
}

/// Encrypt `plaintext` under a 32-byte `key`, binding `aad`; returns a versioned envelope.
#[wasm_bindgen]
pub fn aead_seal(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(sotto_core::aead::seal(&key32(key)?, plaintext, aad))
}

/// Decrypt a versioned envelope under a 32-byte `key`, verifying `aad`.
#[wasm_bindgen]
pub fn aead_open(key: &[u8], envelope: &[u8], aad: &[u8]) -> Result<Vec<u8>, JsError> {
    sotto_core::aead::open(&key32(key)?, envelope, aad).map_err(|e| JsError::new(&e.to_string()))
}
