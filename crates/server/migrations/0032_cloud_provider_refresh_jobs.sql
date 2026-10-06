-- Durable provider refresh work.  The event receipt and invalidation association remain the
-- source of truth; this table is a restart-safe delivery queue for work that must be collected.

CREATE TABLE cloud_provider_refresh_jobs (
    job_id                  TEXT NOT NULL CHECK (btrim(job_id) <> ''),
    provider_namespace      TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id     TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment    TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id                TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    beneficiary_id          TEXT NOT NULL CHECK (btrim(beneficiary_id) <> ''),
    allocation_id           TEXT NOT NULL CHECK (btrim(allocation_id) <> ''),
    coverage_source_id      TEXT NOT NULL CHECK (btrim(coverage_source_id) <> ''),
    status                  TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'leased', 'completed', 'poisoned')),
    attempt_count           INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_owner             TEXT,
    lease_expires_at        TIMESTAMPTZ,
    last_error_code         TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at            TIMESTAMPTZ,
    PRIMARY KEY (job_id),
    CONSTRAINT cloud_provider_refresh_job_event_fk
        FOREIGN KEY (provider_namespace, provider_account_id, provider_environment, event_id)
        REFERENCES cloud_provider_event_receipts
            (provider_namespace, provider_account_id, provider_environment, event_id)
        ON DELETE CASCADE,
    CONSTRAINT cloud_provider_refresh_job_lease_pair_check
        CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT cloud_provider_refresh_job_completed_check
        CHECK ((status = 'completed') = (completed_at IS NOT NULL)),
    CONSTRAINT cloud_provider_refresh_job_active_lease_check
        CHECK (status = 'leased' OR (lease_owner IS NULL AND lease_expires_at IS NULL))
);

CREATE UNIQUE INDEX cloud_provider_refresh_jobs_event_idx
    ON cloud_provider_refresh_jobs
        (provider_namespace, provider_account_id, provider_environment, event_id);

CREATE INDEX cloud_provider_refresh_jobs_due_idx
    ON cloud_provider_refresh_jobs (available_at, created_at, job_id)
    WHERE status IN ('pending', 'leased');

CREATE INDEX cloud_provider_refresh_jobs_lease_idx
    ON cloud_provider_refresh_jobs (lease_expires_at, job_id)
    WHERE status = 'leased';
