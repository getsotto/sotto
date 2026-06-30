# Contributing to Sotto

Thank you for your interest in contributing to Sotto.

Sotto is an early-stage, multi-crate Rust workspace for a zero-knowledge secret sync tool. The project is currently under active development and most behavior is still scaffolding, so contribution guidance is focused on code quality, tests, and safe collaboration.

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

## Branches and pull requests

- Keep branches focused on a single change or issue.
- Rebase or merge from the main branch before opening a pull request to keep your branch current.
- Prefer descriptive branch names and PR titles.
- Link issues or RFC discussions from the PR description.

## Issues

- Search existing issues before opening a new one.
- Use clear, specific titles and reproduction steps where possible.
- For bug reports, include the command you ran, the expected behavior, and the observed behavior.
- For design or feature discussions, describe the problem and a proposed approach.

## Coding standards

- Follow Rust idioms and keep code readable.
- Prefer explicit error handling and clear type boundaries.
- Use existing crate abstractions when possible.
- Keep contributions aligned with the repository's architecture:
  - `crates/core` — shared cryptographic types and implementation.
  - `crates/cli` — native command-line interface.
  - `crates/server` — API server and sync backend.
  - `crates/wasm` — browser/WebAssembly bindings.

## Formatting and linting

Run formatting and lint checks before submitting a PR.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Tests

- Add tests for new behavior and regressions.
- Run the workspace test suite locally:

```sh
cargo test --workspace
```

- When working on server or integration behavior, use the existing crate test harnesses.

## Supply-chain policy

This repository includes `deny.toml` for dependency and license checks. Validate the supply-chain policy locally with:

```sh
cargo deny check
```

## Security

- Sotto is a cryptographic project, and security is a first-class concern.
- Do not introduce unstable or unaudited cryptography without a strong review.
- If you discover a security issue, please report it privately if possible.

## License

No license has been selected for the workspace yet, and all crates are currently marked `publish = false`.

If you are contributing, be aware that the current repository does not have a finalized license. Contributions are accepted under the repository's current governance and will be subject to the final license decision.

## Notes

- This project is not production-ready.
- The current implementation is early scaffolding and should not be used to protect real secrets.
