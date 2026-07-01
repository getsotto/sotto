-- Per-member environment vault-key grants (M5 PR3b).
--
-- A grant is the environment's vault key sealed (X25519 anonymous box) to a member's public key —
-- server-opaque ciphertext, useless without that member's private key. This table is what lets a
-- teammate *decrypt* a shared environment (access, from PR3a, only lets them see the ciphertext).
--
-- Until now a single grant lived inline on `environments.enc_vault_key`, sealed to the creator. That
-- column is retained for the current client read path; going forward every grant (including the
-- creator's, written at environment creation) also lands here, so "fetch my grant" is uniform.

CREATE TABLE IF NOT EXISTS environment_grants (
    env_id        TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    user_id       TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- The env vault key sealed to `user_id`'s public key.
    enc_vault_key BYTEA NOT NULL,
    -- Who issued the grant (audit/provenance only); NULLed if that user is deleted.
    granted_by    TEXT REFERENCES users (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (env_id, user_id)
);

-- "Which environments am I granted?" — the per-user read path.
CREATE INDEX IF NOT EXISTS environment_grants_user_idx ON environment_grants (user_id);

-- Backfill the creator's grant for every existing environment. The inline `enc_vault_key` was
-- sealed to whoever created the env; for all existing (personal-project) data that is the project
-- owner, so key the backfilled row to `owner_id`. Org projects are new (PR3a) with no real data.
INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by)
SELECT e.id, p.owner_id, e.enc_vault_key, p.owner_id
FROM environments e
JOIN projects p ON e.project_id = p.id
ON CONFLICT (env_id, user_id) DO NOTHING;
