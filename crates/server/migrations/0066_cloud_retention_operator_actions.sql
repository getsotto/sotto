-- Audit protected review actions independently from the retention job lifecycle. The actor label
-- is operator-supplied text authenticated only by the shared bearer token; it is not an identity
-- assertion. No action in this table can authorise purge.

ALTER TABLE cloud_retention_jobs
    ADD COLUMN IF NOT EXISTS operator_fingerprint TEXT
        GENERATED ALWAYS AS (md5(job_id)) STORED;

CREATE UNIQUE INDEX IF NOT EXISTS cloud_retention_jobs_operator_fingerprint_idx
    ON cloud_retention_jobs (operator_fingerprint);

CREATE TABLE IF NOT EXISTS cloud_retention_operator_actions (
    action_id       TEXT PRIMARY KEY,
    job_id          TEXT NOT NULL REFERENCES cloud_retention_jobs (job_id) ON DELETE RESTRICT,
    action          TEXT NOT NULL,
    actor_label     TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (action IN ('held', 'cancelled', 'retried')),
    CHECK (length(trim(actor_label)) BETWEEN 1 AND 80)
);

CREATE INDEX IF NOT EXISTS cloud_retention_operator_actions_job_idx
    ON cloud_retention_operator_actions (job_id, created_at DESC, action_id);

CREATE TABLE IF NOT EXISTS cloud_retention_scan_state (
    scan_key          TEXT PRIMARY KEY,
    cursor_user_id    TEXT,
    lease_owner       TEXT,
    lease_expires_at  TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scan_key = 'personal_expiry'),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL))
);

INSERT INTO cloud_retention_scan_state(scan_key) VALUES('personal_expiry')
ON CONFLICT (scan_key) DO NOTHING;

CREATE INDEX IF NOT EXISTS cloud_retention_scan_lease_idx
    ON cloud_retention_scan_state (lease_expires_at)
    WHERE lease_expires_at IS NOT NULL;
