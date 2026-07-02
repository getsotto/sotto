# Security

Sotto is **pre-1.0 and has not undergone a third-party cryptographic audit.** Evaluate it
accordingly. This document describes the intended security model and its honest limitations.

## The guarantee: end-to-end encrypted, zero-knowledge

Secrets are encrypted on the client under keys derived from your **master password** and a
high-entropy **secret key**. Neither ever leaves your device (CLI) or browser tab (web), and the
server cannot derive them. The server stores **ciphertext plus minimal metadata** and can never read
your secret names or values.

- **Key hierarchy:** `master key = Argon2id(password, salt) combined with the secret key` → seals
  your account **X25519 keypair** → opens per-environment **vault-key grants** (the vault key
  sealed to each member's — or machine token's — public key) → the vault key wraps a per-secret
  **data key** → encrypts the name and value. Sharing seals the vault key to a teammate's public
  key; removing a member rotates the vault key and rewraps the data keys.
- **One audited implementation:** the crypto lives in `sotto-core` (Rust) and runs identically in
  the CLI (native) and the browser (WASM). A cross-implementation gate pins byte-for-byte test
  vectors so the two builds can't silently diverge.
- **Associated data (AAD)** binds every ciphertext to its location (environment, secret, version,
  field), so the server cannot swap, relocate, or mix blobs undetected.

## What the server *can* see (metadata)

Zero-knowledge covers **contents, not all metadata.** An operator or a database thief can observe:
your email and OAuth identity, the sharing/grant graph, approximate secret **sizes**, timestamps,
and per-environment revisions. Names and values are never exposed. A full database theft is not
brute-forceable, because the secret key is never stored server-side.

## Assurance surfaces

- **CLI — high assurance.** A native binary with a minimal, `cargo-deny`-audited dependency set and
  keys held in the OS keychain / process memory. This is the recommended surface for sensitive use.
- **Web — convenience, weaker.** The web client is **re-fetched from the server on every load**, so
  a server or host compromise can serve tampered code. It is hardened but is not the high-assurance
  path:
  - Strict **Content-Security-Policy** (no inline scripts, no third-party origins) and
    **Subresource Integrity** on all emitted assets.
  - The session is an **httpOnly, `Secure`, `SameSite=Lax` cookie** — not readable by JavaScript,
    so XSS cannot exfiltrate it (and it grants only ciphertext access regardless).
  - The master key and decryption keys exist **only in memory** for the tab's lifetime; nothing
    secret is written to `localStorage`.

  XSS in the served client is the primary residual web risk; we mitigate it and document the weaker
  posture rather than hide it.

## Verifying releases

Tagged releases ship tarballs, a `SHA256SUMS` file, and Sigstore signatures. Signing is
**keyless**: each artifact's `.sigstore.json` bundle binds it to this repository's release
workflow identity via GitHub OIDC — there is no long-lived signing key to steal. To verify:

```sh
sha256sum --check --ignore-missing SHA256SUMS
cosign verify-blob \
  --bundle sotto-<version>-<target>.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github.com/getsotto/sotto/.github/workflows/release.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  sotto-<version>-<target>.tar.gz
```

The full threat model — adversaries, guarantees, and explicit non-goals — is published in
[THREAT-MODEL.md](./THREAT-MODEL.md).

## Not yet in place

Apple notarization, reproducible builds, key transparency for the server's public-key
distribution, and a third-party audit are on the roadmap, not shipped. Until then, trust decisions
should reflect an unaudited pre-1.0 project.

## Reporting a vulnerability

Please report suspected vulnerabilities privately rather than opening a public issue. Email
**security@getsotto.dev** (or open a GitHub security advisory). We aim to acknowledge reports within
a few business days.
