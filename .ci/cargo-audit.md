# Lockfile audit

The `supply-chain` CI job audits the whole lockfile with `scripts/check-cargo-audit`. Use Python
3.11 or newer; CI uses Python 3.12. The checker requires cargo-audit exactly 0.22.2:

```sh
cargo install cargo-audit --version 0.22.2 --locked
python3 -B -m unittest discover -s scripts/tests -v
scripts/check-cargo-audit
```

The [audit policy](cargo-audit-policy.toml) records exact exceptions for dormant `rsa`, `spin` and
unused keyring async-backend lock entries. Each must match its `package`, `version`, `source`,
`kind` and `finding`.
It must have no `normal`, `build` or `dev` dependency path across all targets, with default and
all workspace features. Cargo's `dev` edges correspond to `dev-dependencies`.

New findings, changed identities, reachable packages, failed scans and stale exceptions fail CI.
Remove an exception in the same PR that removes its finding; an empty exception list requires a
clean audit. The checker exits 0 on success and 1 on policy or scan failure. Raw `cargo audit`
still reports the dormant findings; the exceptions do not fix vulnerabilities or change package
maintenance status.

Project [audit defaults](../.cargo/audit.toml) keep advisory fetching and yanked checks enabled and
override a developer's global audit configuration. Do not add advisory ignores or scan filters.

JSON mode can silently skip yanked checks in cargo-audit 0.22.2. The checker therefore requires
an independent terminal-format scan with no registry failures and matching finding identities.
This establishes a separate successful scan; it does not prove the earlier JSON scan completed.
