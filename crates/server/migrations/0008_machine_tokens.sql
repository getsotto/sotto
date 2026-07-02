-- Machine identities (M5 PR5a): per-environment tokens for CI / service access.
--
-- A machine token binds exactly one environment. The row carries the machine's X25519 public key
-- and the env vault key sealed to it (its grant) — the grant is 1:1 with the token, so it lives
-- here rather than in `environment_grants`. The server stores only the BLAKE2b hash of the API
-- token; the raw token string (which also carries the machine's private key, never sent to the
-- server) is shown to the creator exactly once. Zero-knowledge holds: the server can authenticate
-- the machine but can never decrypt anything.
--
-- Revocation is a tombstone (`revoked_at`), so the row remains for audit; a revoked token fails
-- authentication immediately. Rotating the environment must re-seal every *active* token's grant
-- (enforced by the rotate endpoint), so rotation never silently strands CI on the old key.

CREATE TABLE IF NOT EXISTS machine_tokens (
    id            TEXT PRIMARY KEY,
    env_id        TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    -- Human label ("github-actions", "deploy-bot") for listings.
    name          TEXT NOT NULL,
    token_hash    BYTEA NOT NULL UNIQUE,
    public_key    BYTEA NOT NULL,
    -- The env vault key sealed to `public_key` (this token's grant).
    enc_vault_key BYTEA NOT NULL,
    created_by    TEXT REFERENCES users (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS machine_tokens_env_idx ON machine_tokens (env_id);
