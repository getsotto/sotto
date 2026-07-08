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
browser, and share a single secret via a one-time link. Teams work end to end too: organizations
with roles, per-member environment grants, key rotation on member removal, machine tokens for CI,
and lost-key account recovery.

| Component | Available now |
| --- | --- |
| Crypto core | KDF, XChaCha20-Poly1305 AEAD + AAD, key wrapping, X25519 sealed-box grants, the environment vault hierarchy, data-key rewrap (rotation), share-link crypto, and key encoding — with native↔WASM golden vectors |
| CLI | `init`, local secret management, `run`-style injection, `login`/`push`/`pull` sync, new-device `setup`, `share`; teams: `org create/ls/invite/members/remove`, `grant`, `clone`, `rotate`, machine `token create/ls/revoke` (with `SOTTO_TOKEN` mode for CI), lost-kit `reset` |
| Server | OAuth login + sessions, account + snapshot sync (versioned writes, ETag), orgs + memberships + roles, per-member vault-key grants, transactional key rotation, machine tokens, account reset, and share links — ciphertext only |
| Web | Login (cookie session), in-browser unlock + vault decryption via your own grant, one-time share create/receive, and a team panel: orgs, members, invite by email, share an environment with a member |

## Install

Prebuilt, signed binaries for macOS (Apple Silicon + Intel) and Linux x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

The installer verifies the tarball's SHA-256 checksum — and its Sigstore signature, when `cosign`
is installed — before installing to `~/.local/bin`. Prefer to look first? Grab a tarball from the
[releases page](https://github.com/getsotto/sotto/releases) and verify it manually per
[SECURITY.md](SECURITY.md), or build from source (see [Developing](#developing)).

## Quick start

```sh
sotto init                   # create your identity + first project — SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto run -- npm start       # inject the environment's secrets into any command
sotto login && sotto push    # optional: sync ciphertext via the hosted instance (getsotto.co.uk)
sotto share DATABASE_URL     # one-time, burn-after-reading link for a single secret
```

`sotto login` uses the hosted instance at [getsotto.co.uk](https://getsotto.co.uk) unless you point
it elsewhere with `--server <url>` (see [Deploying](deploy/README.md) to run your own). Either way
the server only ever stores ciphertext — the web vault at the same address decrypts in your
browser, with keys that never leave your devices.

Working with a team:

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user
sotto grant <user-id>                      # share the active environment (they run `sotto clone`)
sotto token create --name ci               # SOTTO_TOKEN: run/export in CI, no password needed
```

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

## Developing

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

GitHub OAuth login requires `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. For any non-local
deployment also set `SOTTO_PUBLIC_URL` to the server's externally reachable origin — it builds the
GitHub callback URL and must match the OAuth app's registered callback (it otherwise defaults to
`http://localhost:8080`) — and, for the web client, `SOTTO_WEB_ORIGIN`. Without OAuth the server
still boots (serving `/health` and running migrations), but login and every authenticated endpoint
(sync, share creation) are unavailable.

### Web client

The browser client runs the same crypto core via WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev      # dev server (proxies the API to localhost:8080)
npm run build    # production bundle → web/dist (strict CSP + Subresource Integrity)
```

### Deploying

One command brings up a complete hosted instance — Postgres, the server, and Caddy with automatic
HTTPS — from [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); the runbook is
[`deploy/README.md`](deploy/README.md). The pieces also work standalone: serve the web app and API
from **one origin** (so the session cookie and CSP stay same-origin) — the included
[`Caddyfile`](Caddyfile) serves `web/dist` and reverse-proxies the API, with security headers;
[`Dockerfile`](Dockerfile) builds the server image (migrations run on boot).

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

## Telemetry

The **server** sends one anonymous ping per day (the first 10–20 minutes after boot) to
`https://getsotto.co.uk/telemetry/v1/ping`, so we can count active instances and see which
versions are in the wild. The response names the latest release, and the server logs a line when
it is running an outdated version. This is the **entire** payload — the sending code is
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), and a unit test pins the
payload to exactly these four fields:

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` is a random UUID generated once and stored in your database — derived from nothing,
so it identifies no hardware, host, or account; deleting it makes the instance a fresh anonymous
counter. The ingest side stores no IP addresses and no derived location. There are no org, member,
or secret counts, and no usage events. The **CLI, web client, and WASM never send anything**.

Opt out with `SOTTO_TELEMETRY=off` (or the cross-tool
[`DO_NOT_TRACK=1`](https://consoledonottrack.com)) — when disabled the task is never started and
no request is ever made. `SOTTO_TELEMETRY_URL` redirects the ping (e.g. to aggregate a private
fleet), and records idle for 12 months are purged from the hosted census.

## Security

Sotto's model is zero-knowledge: plaintext secrets and usable decryption keys stay on client
devices, and the server sees only ciphertext plus minimal metadata. This is implemented but **not
yet independently audited** — see [SECURITY.md](SECURITY.md) for the model, the honest metadata
exposure, how the (re-fetched, weaker) web surface is hardened, and how to verify signed releases.
The full adversary model, guarantees, and explicit non-goals are published in
[THREAT-MODEL.md](THREAT-MODEL.md). Report vulnerabilities privately per SECURITY.md.

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — all crates and the web client.
You may not use this project except in compliance with the License. Unless required by
applicable law or agreed to in writing, software distributed under the License is distributed
on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND.
