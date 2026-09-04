# Security audit scope and readiness

> Status: pre-audit. This document describes the proposed scope and the evidence required for
> an independent review. It is not an audit report or a security certification.

## Objective

Establish whether Sotto's end-to-end encryption protocol and its critical key-lifecycle paths
provide the confidentiality, integrity, revocation, and recovery properties described in
[`THREAT-MODEL.md`](THREAT-MODEL.md), and identify implementation or integration flaws that could
break those properties.

The audit target will be an immutable release candidate selected after the readiness work below.
Changes to the cryptographic scheme or key lifecycle will be frozen during the engagement unless
the auditor explicitly agrees to expand the scope.

## Proposed scope

### Phase 1: protocol and cryptographic implementation

- `crates/core`: KDF, key derivation, envelope formats, AEAD and AAD construction, key wrapping,
  vault hierarchy, data-key rewrapping, share-link crypto, encoding, and secret handling.
- `crates/wasm`: bindings and the native-to-WASM cross-implementation boundary.
- Protocol flows that use the core: account initialisation, new-device setup, recovery, grants,
  member removal and rotation, and one-time share links.
- Versioning, downgrade resistance, malformed-input handling, and fail-closed behaviour.

### Phase 2: critical security integration

Where the budget and auditor expertise allow, the same engagement should also cover the code that
can defeat a correct cryptographic core:

- server grant distribution, access checks, sync versioning, and atomic rotation;
- CLI key storage, session handling, export/reveal paths, and machine-token scope;
- authentication, session cookies, CSRF protection, and security-sensitive web flows;
- release and deployment paths that deliver the CLI, server image, and browser client.

If Phase 2 is not included in the first engagement, Sotto will describe the result as a Phase 1
cryptographic-core audit and will commission a separate integration review before making a broader
production-security claim.

## Claims to validate

The review should explicitly test whether:

1. A server, network attacker, or database thief cannot decrypt secret names or values without
   client-held key material.
2. AAD and version binding make relocation, substitution, and rollback fail closed.
3. Grants expose an environment vault key only to the intended member or machine identity.
4. Removing a member prevents access to future writes and changed secrets after rotation.
5. Recovery and new-device setup preserve the intended key hierarchy without creating a server-side
   decryption path.
6. Share links preserve their one-time, expiry, and fragment-key boundaries.
7. Native and WASM implementations preserve the same wire and cryptographic semantics.

The report must also identify claims that are not supported, including cached plaintext on a
previously authorised device, metadata exposure, server-side key-directory trust before key
transparency, and the weaker assurance of the served browser client.

## Evidence for the auditor

- [`SECURITY.md`](SECURITY.md) and [`THREAT-MODEL.md`](THREAT-MODEL.md)
- maintainer-provided protocol and data-model specifications, supplied as part of the audit package
- native, server, CLI, and WASM tests, including cross-implementation vectors
- property tests, malformed-input tests, fuzzing corpus and fuzzing results
- dependency, licence, vulnerability, and release-build records
- CI workflow permissions, release signing, provenance, and deployment documentation
- a list of known limitations, previous reviews, and accepted risks

## Required engagement outputs

- a public, stand-alone report naming the exact commit and components reviewed;
- findings with severity, affected versions, exploitability, and recommended remediation;
- coordinated disclosure support for security-relevant findings;
- maintainer fixes for agreed findings;
- auditor review of the fixes, with the result recorded in the public report or an addendum;
- a release note and customer-facing summary that distinguish audited scope from unaudited scope.

## Readiness gates

Before the audit starts, Sotto should:

- freeze and review the protocol specification and threat model;
- add fuzzing at parsing, decoding, envelope, grant, rotation, and share-link boundaries;
- require protected, reviewed changes and passing security checks on `main`;
- enable dependency update, secret-scanning, and vulnerability monitoring;
- pin build and release actions and record dependency provenance;
- verify CLI/config file permissions and Docker build-context exclusions;
- rehearse backup restore and record the hosted-service incident and disclosure procedure;
- resolve or explicitly accept every known high-severity issue.

## Post-audit release rule

Sotto will not describe itself as “fully audited”. It will publish the audited release, scope,
commit, findings, remediation status, and residual risks. A broader assurance claim requires the
corresponding integration and operational review, not just a clean Phase 1 report.
