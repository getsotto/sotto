# Sotto

End-to-end encrypted secret sync for developer teams. Stop Slacking your `.env`.

> [!WARNING]
> Sotto is in early development. The repository currently contains milestone M0
> scaffolding, not a working secret manager. Do not use it to protect production secrets.

Sotto is being designed around one Rust crypto implementation shared by the native CLI
and browser clients through WebAssembly. The server will store and synchronize encrypted
data without receiving plaintext secrets.

## Current status

The workspace builds and tests, but most product behavior is still represented by stubs.

| Component | Available now | Planned |
| --- | --- | --- |
| Crypto core | Scheme version and format definitions | KDF, encryption, envelopes, key wrapping, and key encoding |
| CLI | `init` and `run` command surface | Local secret management and environment injection |
| Server | `GET /health` endpoint | Encrypted snapshot sync, grants, versioned writes, and rotation |
| WASM | Exposes the shared scheme version | Browser encryption and decryption bindings |

## Architecture

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

The workspace contains four crates:

- `crates/core` — shared cryptographic types and, the complete crypto implementation.
- `crates/cli` — the `sotto` command-line interface and primary native client.
- `crates/server` — the Axum-based synchronization API.
- `crates/wasm` — `wasm-bindgen` bindings that expose the core to web clients.

## Prerequisites

- [Rustup](https://rustup.rs/) with stable Rust 1.89 or newer
- The `clippy` and `rustfmt` components
- The `wasm32-unknown-unknown` target

The checked-in `rust-toolchain.toml` asks Rustup to install the required components and
target automatically.

## Getting started

Clone the repository, then build and test the complete workspace:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

Inspect the current CLI:

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init
cargo run -p sotto-cli -- run -- your-command --with-arguments
```

The commands are intentionally non-functional stubs until milestone M2.

Run the development server:

```sh
cargo run -p sotto-server
```

In another terminal, verify its health endpoint:

```sh
curl http://127.0.0.1:8080/health
```

The response should be:

```text
ok
```

## Development checks

Run the same core checks used by CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Supply-chain policy is defined in `deny.toml` and checked in CI with
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny):

```sh
cargo deny check
```

The WASM crate currently builds for the host as part of the workspace. The
`wasm32-unknown-unknown` cross-build is deferred, when the `getrandom` WASM
backend is configured.

The compatibility gate is native encryption and WASM decryption producing matching,
byte-for-byte results from shared test vectors.

## Security

Sotto's intended security model is zero knowledge: plaintext secrets and usable
decryption keys remain on client devices. That model is not implemented or audited yet.
The current code must not be treated as production-ready cryptographic software.

## License

No license has been selected yet, and all crates are marked `publish = false`.
The intended model is open core, with a permissive license being considered for the
client and crypto core and a separate decision pending for the server.
