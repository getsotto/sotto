# Contributing to Sotto

Thank you for your interest in contributing to Sotto.

Sotto is a zero-knowledge secret sync tool: one Rust crypto core, a native CLI, an Axum
server that stores ciphertext only, and a browser client that runs the same core through
WebAssembly. The end-to-end flow works (encrypt locally, sync, decrypt on another device,
share a one-time link, team grants and rotation). It is pre-1.0 and has not had a
third-party cryptographic audit - see [SECURITY.md](SECURITY.md).

The fastest way in is a labelled
[good first issue](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22).
Questions that are not bugs belong in
[Discussions](https://github.com/getsotto/sotto/discussions).

## Ways in

You do not need to touch cryptography to help.

- **Docs and copy.** README, CLI `--help` text, and the landing page. Prose uses British
  English (organisation, initialise, behaviour) and plain hyphens, never em dashes.
- **CLI affordances.** Completions already exist (`sotto completions bash`); wiring them
  into `install.sh`, adding examples to `--help`, and Windows PATH notes are typical
  first issues.
- **Tests and fixtures.** Anything that makes a failure mode obvious without expanding
  the crypto surface.
- **Crypto and the server** are welcome too, but start a discussion or issue first.
  Unaudited cryptography is a first-class review concern.

Search existing issues before opening a new one. Security reports go privately per
[SECURITY.md](SECURITY.md), never as a public issue.

## Getting started

1. Fork the repository and clone your fork.
2. Install Rust with Rustup and ensure stable Rust 1.89 or newer is active.
3. Install the required toolchain components:

```sh
rustup component add clippy rustfmt
rustup target add wasm32-unknown-unknown
```

4. Build the workspace:

```sh
cargo build --workspace
```

5. Run the full test suite:

```sh
cargo test --workspace
```

## Frontend development

The web client uses Node.js 22 and wasm-pack 0.15.0, alongside the Rust and
`wasm32-unknown-unknown` prerequisites above. Install wasm-pack using the
[official project instructions](https://github.com/wasm-tool/wasm-pack#installation), then run
the following from the repository root:

```sh
cd web
npm ci
npm run build:wasm
npm run typecheck
```

`npm run build:wasm` generates the WebAssembly bindings required before
typechecking. `npm run dev` generates those bindings and starts the Vite
development server. `npm run build` generates bindings and creates a production
bundle. `npm run smoke` generates Node bindings and checks that the crypto core
loads.

To run the development server against the local API, first follow the
[server setup](README.md#running-the-server) in the README. Browser regression
tests also need Postgres, Playwright and the e2e fixture described in
[web/e2e/README.md](web/e2e/README.md); viewing the landing page does not
exercise authenticated vault flows.

## Branches and pull requests

- Keep branches focused on a single change or issue.
- Rebase or merge from the main branch before opening a pull request to keep your branch current.
- Prefer descriptive branch names and PR titles.
- Link issues or RFC discussions from the PR description.

## Issues

Use the issue templates. Search existing issues first. Bug reports need the command, the
expected behaviour, and the observed behaviour. Feature ideas start with the problem, not
the patch.

## Coding standards

- Follow Rust idioms and keep code readable.
- Prefer explicit error handling and clear type boundaries.
- Use existing crate abstractions when possible.
- Keep contributions aligned with the repository's architecture:
  - `crates/core` - shared cryptographic types and implementation.
  - `crates/cli` - native command-line interface.
  - `crates/server` - API server and sync backend.
  - `crates/wasm` - browser/WebAssembly bindings.

## Formatting and linting

Prose - comments, docs, UI copy, and error messages - uses British English spelling
(organisation, initialise, behaviour) and plain hyphens or commas, never em dashes.
Identifiers, wire-format keys, SQL schema names, and external API tokens (e.g. the
`Authorization` header, serde's `Serialize`) keep their canonical spelling.

Run formatting and lint checks before submitting a PR.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Translating the README

The root README ships in English plus translations. English (`README.md`) is
authoritative and wins on any discrepancy; each translation says so in its header.

- Available now: Español (`README.es.md`), Français (`README.fr.md`), Deutsch
  (`README.de.md`), Português (BR) (`README.pt-BR.md`).
- Translate prose only. Keep fenced code blocks, commands, identifiers, URLs,
  link destinations, inline code, heading levels, tables, and admonition markers
  exactly as in the English file.
- Keep the language nav bar at the top of every README file, and link a new
  translation from all of them.
- Run `scripts/check-readme-i18n` before opening a PR; CI runs it with its unit
  tests. To add a language, register the file in `TRANSLATIONS` in that script.
- Translations are best-effort: reviewers fluent in the language are welcome,
  especially for the security wording.

## Tests

- Add tests for new behaviour and regressions.
- Run the workspace test suite locally:

```sh
cargo test --workspace
```

- When working on server or integration behaviour, use the existing crate test harnesses.

- Run the script policy tests too (Python 3.11 or newer; CI uses 3.12):

```sh
python3 -B -m unittest discover -s scripts/tests -v
```

## Supply-chain policy

This repository includes `deny.toml` for dependency and licence checks. Validate the supply-chain policy locally with:

```sh
cargo deny check
```

Run `scripts/check-cargo-audit` too (Python 3.11 or newer and `cargo-audit` exactly 0.22.2).
CI uses Python 3.12.
See [Development checks](README.md#development-checks) for installation, policy tests and
the rules for reviewing or removing dormant lockfile exceptions.

## Security

- Sotto is a cryptographic project, and security is a first-class concern.
- Do not introduce unstable or unaudited cryptography without a strong review.
- If you discover a security issue, please report it privately if possible.

## Licence

Sotto is licensed under the [Apache License 2.0](LICENSE). By contributing, you agree that your contributions will be licensed under the same licence.

## Developer Certificate of Origin

Every commit must be signed off under the [Developer Certificate of Origin](https://developercertificate.org) (DCO). Signing off certifies that you wrote the change or otherwise have the right to submit it under the project's licence.

Add a `Signed-off-by` line to each commit with the `-s` flag:

```sh
git commit -s
```

This appends a line like `Signed-off-by: Your Name <you@example.com>` using your git identity. If a branch already has unsigned commits, fix them with:

```sh
git rebase --signoff main
```

Pull requests with unsigned commits cannot be merged.

## Notes

- Sotto is pre-1.0 and has not had a third-party cryptographic audit. Honest guidance:
  useful for team development and staging secrets today; keep production crown jewels
  elsewhere until the audit. See [SECURITY.md](SECURITY.md) and [THREAT-MODEL.md](THREAT-MODEL.md).
