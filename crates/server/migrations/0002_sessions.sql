-- Authentication state: human OAuth sessions, plus short-lived OAuth login (CSRF) state.
--
-- These are auth credentials, not secret content, so storing them server-side does not weaken
-- the zero-knowledge guarantee. Sessions store only the BLAKE2b *hash* of the bearer token, so a
-- database leak never yields a usable token.

CREATE TABLE IF NOT EXISTS sessions (
    token_hash   BYTEA PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_user_id_idx ON sessions (user_id);

-- One row per in-flight `secrets login`: binds the server-issued OAuth `state` (CSRF token) to
-- the CLI's loopback redirect target. Consumed (deleted) on callback; stale rows are ignored by
-- the freshness check.
CREATE TABLE IF NOT EXISTS oauth_logins (
    state            TEXT PRIMARY KEY,
    cli_redirect_uri TEXT NOT NULL,
    cli_state        TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
