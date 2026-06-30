// Loads the Sotto crypto core (the same Rust core as the CLI, compiled to WASM) into the browser.
//
// The `.wasm` binary is imported as a URL asset so Vite fingerprints + serves it; `init` fetches
// and instantiates it. `loadWasm()` is idempotent — the first call initializes, the rest await it.

import init, { scheme_version } from "./wasm/sotto_wasm.js";
import wasmUrl from "./wasm/sotto_wasm_bg.wasm?url";

let ready: Promise<void> | null = null;

export function loadWasm(): Promise<void> {
  if (!ready) {
    ready = init({ module_or_path: wasmUrl }).then(() => undefined);
  }
  return ready;
}

export { scheme_version };
