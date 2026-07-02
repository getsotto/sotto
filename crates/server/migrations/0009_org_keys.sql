-- Per-org keys + the inline vault-key column retirement (M6 PR1).
--
-- `enc_org_key` is the org's symmetric key sealed (X25519 box) to this member's public key —
-- server-opaque, like every grant. It decrypts org/project/environment *display names* for org
-- resources, so every member reads real names instead of record-id fallbacks. NULL means the
-- member hasn't been granted the org key (yet, or their account was reset); clients fall back to
-- displaying ids. Metadata only: secrets are protected by the per-environment vault keys, which
-- rotate on removal — the org key does not (a removed member remembering display names is an
-- accepted, documented leak).
--
-- `environments.enc_vault_key` is dropped: it duplicated the creator's row in
-- `environment_grants` (populated at creation since 0007, backfilled by 0007 before that). The
-- grant table is now the single source; the environment listing returns the caller's own grant.

ALTER TABLE organization_memberships
    ADD COLUMN IF NOT EXISTS enc_org_key BYTEA;

ALTER TABLE environments
    DROP COLUMN IF EXISTS enc_vault_key;
