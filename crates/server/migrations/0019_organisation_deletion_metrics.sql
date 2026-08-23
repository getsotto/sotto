-- Operational counters belong beside the deletion state machine, not in the organisation audit
-- stream. They contain aggregate names and outcomes only, so retries remain measurable without
-- retaining provider messages or organisation identifiers.

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
        CHECK (length(outcome) BETWEEN 1 AND 64)
);
