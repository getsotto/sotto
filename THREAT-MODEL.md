# Threat Model

> What we defend against, what we don't, and why - for engineers, auditors, and security-
> conscious customers. Honesty is the point: an E2EE product that overclaims loses the trust
> it's selling. See [SECURITY.md](./SECURITY.md) for the security model summary and release
> verification instructions.
>
> Last reviewed: 2026-07-02, against the implementation as of that date.

## Assets (in priority order)

1. **Secret values and names** - confidentiality *and* integrity.
2. **Key material** - master key, secret key, vault/data keys, private keys.
3. **Metadata** - sharing graph, sizes, timestamps (partial protection only).
4. **Availability** - access to your own secrets.

## Trust boundaries

| Component | Trust | Sees |
|---|---|---|
| **Client** (CLI / WASM) | Trusted - holds keys, does all crypto | plaintext |
| **Server** | *Semi-trusted* - stores ciphertext, enforces access; **cannot decrypt** | ciphertext + metadata |
| **Network** | Untrusted (TLS) | ciphertext in transit |

Core invariant: **the server only ever holds opaque, versioned, AAD-bound ciphertext plus the
key-wrapping graph.** Names and values are always encrypted client-side.

---

## Adversaries × capabilities

| Adversary | Can | Cannot | Mitigation / residual risk |
|---|---|---|---|
| **Malicious/compromised server, cloud provider, or insider** | Read all metadata (sharing graph, sizes, timestamps); delete/withhold blobs (DoS); attempt blob substitution/rollback | Read names or values; forge a member's secrets | AEAD + AAD makes substitution/rollback **fail closed**; full DB theft is unbruteforceable (secret key absent). *Residual:* metadata + availability. |
| **Network MITM** | Observe/block traffic | Read or tamper plaintext | TLS + client-side E2EE (defence in depth) |
| **Stolen DB / backup theft** | Obtain all ciphertext | Decrypt | No secret keys stored server-side; **crypto-shredding** voids erased data even in backups |
| **Stolen/compromised device** | If unlocked: read secrets. If locked: get ciphertext + keychain-stored secret key | Decrypt without the master password | Argon2id slows password guessing; session TTL + `lock`; `zeroize`; remote token/grant revocation |
| **XSS / malicious served web code** | While the tab is open: read/exfiltrate keys + secrets | Persist beyond the session if hardening holds; touch the CLI surface | Strict CSP, **zero third-party scripts**, SRI, minimal JS, in-memory keys. **CLI is the high-assurance surface; web is convenience** (documented) |
| **Supply-chain attacker** (dep or release) | Ship malicious client code → full compromise of updated users | - (this *is* the worst case) | Minimal audited deps, lockfiles, `cargo audit/deny`, **checksummed + Sigstore-signed releases** (keyless, workflow-identity-bound; see SECURITY.md to verify); Apple notarisation, reproducible builds + transparency on roadmap |
| **Removed / former team member** | Read plaintext they already cached | Read any **new or changed** secret after removal | Instant grant removal (server denial) + **full client-side rotation** (re-encrypt) |
| **Malicious org member (in-scope)** | Access everything granted to them | Access vaults/envs they hold no key for | Env-level keys + grant-graph authz; per-env least privilege |
| **Brute-force attacker (with stolen ciphertext)** | Offline-guess a master password | Make progress without the secret key | Secret key (~128-bit, server-blind) makes DB-only brute force infeasible; Argon2id hardens stolen-device case |
| **Phishing / social engineering** | Trick a user into revealing password/secret key | Bypass crypto if user holds firm | User education; 2FA on the OAuth/session; (future) key-transparency makes impersonation detectable |

---

## Guarantees we make

- **Confidentiality** of names + values against the server, network, and DB/backup theft.
- **Integrity / tamper-evidence** of stored secrets (AEAD + AAD: substitution & rollback fail closed).
- **True revocation** of *future* reads via atomic client-side rotation.
- **Verifiable erasure**, including from backups, via crypto-shredding.

## Explicit NON-goals (we do NOT claim)

- Protecting a device that's **already compromised while unlocked**.
- Recalling **plaintext a member already cached** before removal.
- Hiding **metadata** (sharing graph, approximate sizes, timestamps) from the server.
- Preventing **MITM of a *new* share** - the server is the key directory until **key
  transparency** ships (roadmapped, documented, not hidden).
- Surviving a **compromised client build** beyond what the code-integrity roadmap provides.
- Defending the **web client to the same bar as the CLI** - it is, by construction, weaker.

---

## Roadmap that shrinks the gaps

Key transparency (closes new-share MITM) · reproducible builds + transparency log (shrinks
supply-chain) · signed grants/public keys · hardware-token/passkey unlock · third-party crypto
audit (validates the above).
