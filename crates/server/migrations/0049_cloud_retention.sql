-- Durable, explicitly scoped Cloud retention work.  Jobs are dormant until an operator enables
-- the separate purge switch; the rows preserve the evidence needed to review a dry run first.

CREATE TABLE IF NOT EXISTS cloud_retention_jobs (
    job_id                  TEXT PRIMARY KEY,
    scope_kind              TEXT NOT NULL,
    scope_key               TEXT NOT NULL,
    -- Keep the person identity as retention evidence even after the user row is removed. The
    -- purge path rechecks ownership against live rows before deleting anything.
    subject_user_id         TEXT,
    organisation_id         TEXT REFERENCES organizations (id) ON DELETE RESTRICT,
    eligibility_episode     TEXT NOT NULL,
    deadline_at             TIMESTAMPTZ NOT NULL,
    notice_event_key        TEXT NOT NULL,
    notice_evidence_at      TIMESTAMPTZ,
    dry_run_completed_at    TIMESTAMPTZ,
    expected_coverage_revision BIGINT,
    state                   TEXT NOT NULL DEFAULT 'planned',
    dry_run                 BOOLEAN NOT NULL DEFAULT TRUE,
    purge_enabled           BOOLEAN NOT NULL DEFAULT FALSE,
    hold_code               TEXT,
    attempt_count           INTEGER NOT NULL DEFAULT 0,
    lease_owner             TEXT,
    lease_expires_at        TIMESTAMPTZ,
    last_error_code         TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope_kind IN ('personal', 'shared_organisation', 'orphan_organisation')),
    CHECK (state IN ('planned', 'dry_run', 'ready', 'leased', 'held', 'cancelled', 'completed', 'failed')),
    CHECK (attempt_count >= 0),
    CHECK (length(trim(scope_key)) BETWEEN 1 AND 256),
    CHECK (length(trim(eligibility_episode)) BETWEEN 1 AND 160),
    CHECK (length(trim(notice_event_key)) BETWEEN 1 AND 160),
    CHECK (notice_evidence_at IS NULL OR notice_evidence_at <= deadline_at),
    CHECK (NOT purge_enabled OR NOT dry_run),
    CHECK ((scope_kind = 'personal' AND subject_user_id IS NOT NULL AND organisation_id IS NULL)
        OR (scope_kind IN ('shared_organisation', 'orphan_organisation')
            AND subject_user_id IS NULL AND organisation_id IS NOT NULL)),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS cloud_retention_jobs_identity_idx
    ON cloud_retention_jobs (scope_kind, scope_key, eligibility_episode);

CREATE INDEX IF NOT EXISTS cloud_retention_jobs_due_idx
    ON cloud_retention_jobs (deadline_at, job_id)
    WHERE state IN ('planned', 'ready', 'held');

CREATE TABLE IF NOT EXISTS cloud_retention_scope_items (
    job_id              TEXT NOT NULL REFERENCES cloud_retention_jobs (job_id) ON DELETE CASCADE,
    resource_kind       TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    ownership_kind      TEXT NOT NULL,
    -- Microseconds since epoch make the creation identity exact instead of truncating the
    -- database timestamp to seconds before comparing it during purge.
    expected_created_at BIGINT NOT NULL CHECK (expected_created_at > 0),
    expected_revision   BIGINT,
    state               TEXT NOT NULL DEFAULT 'planned',
    hold_code           TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (job_id, resource_kind, resource_id),
    CHECK (resource_kind IN ('project', 'environment', 'share')),
    CHECK (ownership_kind IN ('personal', 'shared')),
    CHECK (state IN ('planned', 'dry_run', 'deleted', 'held'))
);

CREATE INDEX IF NOT EXISTS cloud_retention_scope_items_state_idx
    ON cloud_retention_scope_items (job_id, state, resource_kind, resource_id);

CREATE TABLE IF NOT EXISTS cloud_retention_receipts (
    receipt_id          TEXT PRIMARY KEY,
    job_id              TEXT NOT NULL REFERENCES cloud_retention_jobs (job_id) ON DELETE RESTRICT,
    resource_kind       TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    action              TEXT NOT NULL,
    deleted_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    tombstone           JSONB NOT NULL,
    UNIQUE (job_id, resource_kind, resource_id, action),
    CHECK (action IN ('deleted', 'held'))
);

CREATE INDEX IF NOT EXISTS cloud_retention_receipts_job_idx
    ON cloud_retention_receipts (job_id, deleted_at);
