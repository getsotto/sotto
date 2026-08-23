-- Operational counters belong beside the deletion state machine, not in the organisation audit
-- stream. They contain aggregate names and outcomes only, so retries remain measurable without
-- retaining provider messages or organisation identifiers.

-- Retain the first transition into purging so completed_at - purge_started_at remains observable
-- after a worker retry or a process restart.
ALTER TABLE organization_deletions
    ADD COLUMN purge_started_at TIMESTAMPTZ;

ALTER TABLE organization_deletions
    ADD CONSTRAINT organization_deletions_purge_started_at_check
        CHECK (purge_started_at IS NULL OR purge_started_at >= requested_at);

CREATE TABLE organization_deletion_metric_counters (
    metric TEXT NOT NULL,
    outcome TEXT NOT NULL,
    value BIGINT NOT NULL DEFAULT 0 CHECK (value >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (metric, outcome),
    -- A fixed label vocabulary prevents raw provider text from becoming an unbounded metric
    -- dimension or retaining details that belong only in sanitised operation state.
    CONSTRAINT organization_deletion_metric_name_check CHECK (
        metric IN (
            'provider_cancellation_attempts',
            'provider_reconciliation_attempts',
            'lease_expiries',
            'stale_compare_and_set',
            'purge_attempts'
        )
    ),
    CONSTRAINT organization_deletion_metric_outcome_check
        CHECK (
            (metric = 'provider_cancellation_attempts'
                AND outcome IN (
                    'terminal', 'missing', 'authentication', 'resource_missing', 'retryable', 'unknown'
                ))
            OR (metric = 'provider_reconciliation_attempts'
                AND outcome IN (
                    'terminal', 'missing', 'blocking', 'authentication', 'resource_missing',
                    'retryable', 'unknown'
                ))
            OR (metric = 'lease_expiries' AND outcome = 'reclaimed')
            OR (metric = 'stale_compare_and_set' AND outcome = 'rejected')
            OR (metric = 'purge_attempts' AND outcome IN ('completed', 'failed'))
        )
);
