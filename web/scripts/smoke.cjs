// Browser-free smoke test: load the Node-targeted WASM build and assert the crypto core runs and
// reports the expected scheme version. The native↔WASM byte-for-byte gate lives in the Rust
// `wasm-pack test --node` cross-impl test; this just proves the web pipeline produced a loadable
// module. Run via `npm run smoke` (which builds the Node target first).

const assert = require("node:assert");
const wasm = require("../.wasm-node/sotto_wasm.js");

const EXPECTED_SCHEME = 1;
const version = wasm.scheme_version();
assert.strictEqual(
  version,
  EXPECTED_SCHEME,
  `scheme_version() returned ${version}, expected ${EXPECTED_SCHEME}`,
);
console.log(`WASM smoke OK — crypto core loaded, scheme v${version}`);
