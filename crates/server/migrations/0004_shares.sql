-- One-time / expiring share links (the viral funnel).
--
-- The server stores only ciphertext (`enc_blob`) plus metadata; the decryption key travels in the
-- URL fragment and never reaches the server, so this stays zero-knowledge. `token` is a
-- high-entropy *public* URL id (not a bearer secret): it gates fetching the ciphertext, which is
-- useless without the fragment key. `view_count` is incremented atomically on fetch to make
-- burn-after-read safe under concurrency.

CREATE TABLE IF NOT EXISTS share_links (
    id              TEXT PRIMARY KEY,
    token           TEXT NOT NULL UNIQUE,
    enc_blob        BYTEA NOT NULL,
    passphrase_salt BYTEA,
    created_by      TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    max_views       INTEGER NOT NULL,
    view_count      INTEGER NOT NULL DEFAULT 0,
    expires_at      TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS share_links_created_by_idx ON share_links (created_by);
