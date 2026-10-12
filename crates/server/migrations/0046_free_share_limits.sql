-- Free hosted links have a bounded active allowance. Legacy rows remain usable with their
-- original behaviour; newly created rows identify their policy class for future billing gates.

ALTER TABLE share_links
    ADD COLUMN IF NOT EXISTS share_class TEXT NOT NULL DEFAULT 'legacy',
    ADD COLUMN IF NOT EXISTS creation_key TEXT,
    ADD COLUMN IF NOT EXISTS creation_hash BYTEA;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conrelid = 'share_links'::regclass
          AND conname = 'share_links_share_class'
    ) THEN
        ALTER TABLE share_links
            ADD CONSTRAINT share_links_share_class
            CHECK (share_class IN ('legacy', 'free', 'paid'));
    END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS share_links_creation_key_idx
    ON share_links (created_by, creation_key)
    WHERE creation_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS share_creation_rate_limits (
    user_id          TEXT PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    window_started_at TIMESTAMPTZ NOT NULL,
    attempt_count    INTEGER NOT NULL,
    CHECK (attempt_count >= 0)
);
