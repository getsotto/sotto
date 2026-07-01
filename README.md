# Sotto

End-to-end encrypted secret sync for developer teams. Stop Slacking your `.env`.

> [!WARNING]
> Sotto is pre-1.0 and has **not** had a third-party cryptographic audit. It works end to end, but
> should not yet be trusted with critical production secrets. See [SECURITY.md](SECURITY.md).

Sotto is built around one Rust crypto implementation shared by the native CLI and the browser
client through WebAssembly. The server stores and synchronizes encrypted data without ever
receiving plaintext secrets or usable keys.

## Current status

The end-to-end flow works: encrypt locally, sync ciphertext, decrypt on another device or in the
browser, and share a single secret via a one-time link.

| Component | Available now |
| --- | --- |
| Crypto core | KDF, XChaCha20-Poly1305 AEAD + AAD, key wrapping, the environment vault hierarchy, share-link crypto, and key encoding — with native↔WASM golden vectors |
| CLI | `init`, local secret management, `run`-style injection, `login`/`push`/`pull` sync, new-device `setup`, and `share` |
| Server | OAuth login + sessions, account + snapshot sync (versioned writes, ETag), and share links — ciphertext only |
| Web | Login (cookie session), in-browser unlock + vault decryption, one-time share create/receive |

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

Use the CLI locally (no server required):

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Secrets are encrypted at rest in a local SQLite store; the master key is cached in the OS keychain
with a TTL. Syncing to a server (`login`/`push`/`pull`/`setup`/`share`) is optional.

### Running the server

The server needs Postgres (a `docker compose up -d` brings one up for local use):

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

GitHub OAuth login requires `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` (and, for the web client,
`SOTTO_WEB_ORIGIN`); without them the server still serves health, sync, and share endpoints.

### Web client

The browser client runs the same crypto core via WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev      # dev server (proxies the API to localhost:8080)
npm run build    # production bundle → web/dist (strict CSP + Subresource Integrity)
```

### Deploying

Serve the web app and API from **one origin** (so the session cookie and CSP stay same-origin). The
included [`Caddyfile`](Caddyfile) serves `web/dist` and reverse-proxies the API, with security
headers; [`Dockerfile`](Dockerfile) builds the server image (migrations run on boot).

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

The cross-implementation gate proves the native and WASM builds agree — native-produced ciphertext
decrypts byte-for-byte in WASM from shared golden vectors:

```sh
wasm-pack test --node crates/wasm
```

The web build and its dependency audit run in CI (`.github/workflows/ci.yml`).

## Security

Sotto's model is zero-knowledge: plaintext secrets and usable decryption keys stay on client
devices, and the server sees only ciphertext plus minimal metadata. This is implemented but **not
yet independently audited** — see [SECURITY.md](SECURITY.md) for the model, the honest metadata
exposure, and how the (re-fetched, weaker) web surface is hardened. Report vulnerabilities privately
per that document.

## License

No license has been selected yet, and all crates are marked `publish = false`.
The intended model is open core, with a permissive license being considered for the
client and crypto core and a separate decision pending for the server.
