// Loads the Sotto crypto core (the same Rust core as the CLI, compiled to WASM) into the browser.
//
// The `.wasm` binary is imported as a URL asset so Vite fingerprints + serves it; `init` fetches
// and instantiates it. `loadWasm()` initializes on the first call and later calls await the same
// promise; a failed init is not cached, so a subsequent call retries instead of staying broken.

import init, {
  aead_open,
  format_decode_key,
  kdf_derive_master_key,
  scheme_version,
  share_open,
  share_passphrase_key,
  share_seal,
  vault_decrypt_name,
  vault_decrypt_value,
  vault_unwrap_key,
} from "./wasm/sotto_wasm.js";
import wasmUrl from "./wasm/sotto_wasm_bg.wasm?url";

let ready: Promise<void> | null = null;

export function loadWasm(): Promise<void> {
  const cached = ready;
  if (cached) {
    return cached;
  }
  const pending = init({ module_or_path: wasmUrl })
    .then(() => undefined)
    .catch((err: unknown) => {
      // Clear the cached promise so the next call retries rather than caching a permanent
      // rejection that would force a full page reload to recover from a transient failure.
      ready = null;
      throw err;
    });
  ready = pending;
  return pending;
}

export {
  aead_open,
  format_decode_key,
  kdf_derive_master_key,
  scheme_version,
  share_open,
  share_passphrase_key,
  share_seal,
  vault_decrypt_name,
  vault_decrypt_value,
  vault_unwrap_key,
};
