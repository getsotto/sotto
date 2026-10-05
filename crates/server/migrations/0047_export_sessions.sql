-- Short-lived, user-scoped export manifests make a multi-request export resumable without
-- trusting a client-supplied list of resources. The manifest contains ciphertext and structural
-- metadata only; it is removed automatically on the next export start after its deadline.

CREATE TABLE IF NOT EXISTS export_sessions (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    manifest      BYTEA NOT NULL,
    manifest_hash BYTEA NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS export_sessions_user_idx
    ON export_sessions (user_id, expires_at);
