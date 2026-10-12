-- Tombstones are copied to a separate append-only backup object before a retention purge can be
-- considered recoverable. This table deliberately has no foreign keys: a journal entry must
-- survive the deletion of its job, owner or organisation row and must be replayable into an older
-- database dump.

CREATE TABLE IF NOT EXISTS cloud_retention_tombstone_journal (
    journal_id          TEXT PRIMARY KEY,
    job_id              TEXT NOT NULL,
    resource_kind       TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    ownership_kind      TEXT NOT NULL,
    expected_owner_id   TEXT,
    expected_created_at BIGINT NOT NULL CHECK (expected_created_at > 0),
    expected_revision   BIGINT,
    action              TEXT NOT NULL,
    tombstone           JSONB NOT NULL,
    recorded_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (resource_kind IN ('project', 'environment', 'share')),
    CHECK (ownership_kind IN ('personal', 'shared')),
    CHECK (action IN ('deleted', 'held')),
    CHECK (ownership_kind <> 'personal' OR expected_owner_id IS NOT NULL),
    UNIQUE (job_id, resource_kind, resource_id, action)
);

CREATE INDEX IF NOT EXISTS cloud_retention_tombstone_journal_order_idx
    ON cloud_retention_tombstone_journal (recorded_at, journal_id);
