# Sotto

End-to-end-encrypted secret sync for developer teams. Stop Slacking your `.env`.

> 🚧 Early development (M0 scaffolding). One audited crypto core in Rust, shared by the CLI
> (native) and the web client (WASM).

## Workspace

| Crate | Role |
|---|---|
| `crates/core` | The crypto core (Rust → native + WASM). Zero-knowledge primitives. |
| `crates/cli` | The `sotto` CLI — the primary, high-assurance surface. |
| `crates/server` | The sync / API backend (axum + Postgres). |
| `crates/wasm` | WASM bindings to the core for the web client. |

## Develop

Prereqs: [Rust](https://rustup.rs) (stable; ≥1.89, required by dryoc).

```sh
cargo build --workspace
cargo test  --workspace
cargo fmt   --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

> The `wasm32-unknown-unknown` cross-build is deferred to M1, when getrandom's `wasm_js`
> backend gets wired (dryoc → rand → getrandom). The WASM crate still builds for the host
> as part of `cargo build --workspace`.

Try the CLI stub:

```sh
cargo run -p sotto-cli -- --help
```

## Status & roadmap

- **M0 — scaffolding** ✅ (this commit): workspace, CI, supply-chain gate.
- **M1 — crypto core + cross-impl test vectors** (next): KDF, versioned envelope,
  XChaCha20-Poly1305 + AAD, X25519 sealed-box wrapping, Crockford key formats. **Exit gate:
  native encrypts ↔ WASM decrypts, byte-for-byte.**
- M2 local CLI · M3 server/sync · M4 web + share links · M5 teams + rotation · M6 monetize.

## License

**TODO** — open-core: the client + crypto core will be permissively licensed (decision
pending: `MIT OR Apache-2.0` vs a protective license for the server). Crates are `publish =
false` until this is set.
