//! WASM bindings to the Sotto crypto core for the web client. See `docs/CRYPTO.md` §8.
//!
//! The point of this crate: the browser runs the *same* crypto core as the CLI, compiled to
//! WASM — one implementation, no second crypto codebase to audit. M1 exposes the real
//! encrypt/decrypt/wrap surface and pins it with cross-implementation test vectors.

use wasm_bindgen::prelude::*;

/// The crypto scheme version baked into this WASM build. The M1 cross-impl test asserts this
/// equals the native core's `SCHEME_VERSION` so the two builds can never silently diverge.
#[wasm_bindgen]
pub fn scheme_version() -> u8 {
    sotto_core::SCHEME_VERSION
}
