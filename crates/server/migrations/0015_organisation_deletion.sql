-- Organisation deletion lifecycle data. The lifecycle flag is kept on the hot organisation row;
-- the operation table stores the durable workflow, lease, billing, and recovery history.

ALTER TABLE organizations
    ADD COLUMN lifecycle_state TEXT NOT NULL DEFAULT 'active',
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD CONSTRAINT organizations_lifecycle_state_check
        CHECK (lifecycle_state IN ('active', 'deleting', 'deleted')),
    ADD CONSTRAINT organizations_lifecycle_deleted_at_check
        CHECK (
            (lifecycle_state = 'deleted' AND deleted_at IS NOT NULL)
            OR (lifecycle_state <> 'deleted' AND deleted_at IS NULL)
        );

-- A deleted organisation is a tombstone, so its encrypted name is cleared at final purge.
ALTER TABLE organizations
    ALTER COLUMN enc_name DROP NOT NULL;

CREATE TABLE organization_deletions (
    id UUID PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    state TEXT NOT NULL
        CHECK (state IN (
            'requested',
            'cancelling_billing',
            'retention',
            'purging',
            'recovering',
            'failed',
            'cancelled',
            'completed'
        )),
    resume_state TEXT
        CHECK (resume_state IS NULL OR resume_state IN (
            'requested',
            'cancelling_billing',
            'retention',
            'purging',
            'recovering'
        )),
    requested_by TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL,
    purge_after TIMESTAMPTZ NOT NULL,
    subscription_id TEXT,
    last_billing_state TEXT
        CHECK (last_billing_state IS NULL OR last_billing_state IN (
            'blocking', 'terminal', 'missing', 'unknown'
        )),
    billing_checked_at TIMESTAMPTZ,
    billing_observation_source TEXT
        CHECK (billing_observation_source IS NULL OR billing_observation_source IN (
            'provider', 'operator'
        )),
    billing_observed_by TEXT,
    billing_observation_reason TEXT,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    next_attempt_at TIMESTAMPTZ,
    last_error_code TEXT,
    state_version BIGINT NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    lease_owner TEXT,
    lease_expires_at TIMESTAMPTZ,
    cancelled_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    managed_backup_expiry_by TIMESTAMPTZ,
    CONSTRAINT organization_deletions_purge_after_check
        CHECK (purge_after >= requested_at),
    CONSTRAINT organization_deletions_resume_state_check
        CHECK ((state = 'failed') = (resume_state IS NOT NULL)),
    CONSTRAINT organization_deletions_billing_result_pair_check
        CHECK ((last_billing_state IS NULL) = (billing_checked_at IS NULL)),
    CONSTRAINT organization_deletions_billing_checked_at_check
        CHECK (billing_checked_at IS NULL OR billing_checked_at >= requested_at),
    CONSTRAINT organization_deletions_lease_pair_check
        CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT organization_deletions_lease_expires_at_check
        CHECK (lease_expires_at IS NULL OR lease_expires_at >= requested_at),
    CONSTRAINT organization_deletions_next_attempt_at_check
        CHECK (next_attempt_at IS NULL OR next_attempt_at >= requested_at),
    CONSTRAINT organization_deletions_backup_expiry_check
        CHECK (managed_backup_expiry_by IS NULL OR managed_backup_expiry_by >= purge_after),
    CONSTRAINT organization_deletions_terminal_timestamp_check
        CHECK (
            (state = 'cancelled') = (cancelled_at IS NOT NULL)
            AND (cancelled_at IS NULL OR cancelled_at >= requested_at)
        ),
    CONSTRAINT organization_deletions_completed_timestamp_check
        CHECK (
            (state = 'completed') = (completed_at IS NOT NULL)
            AND (completed_at IS NULL OR completed_at >= requested_at)
        ),
    CONSTRAINT organization_deletions_observation_actor_check
        CHECK (
            (
                billing_observation_source IS NULL
                AND billing_observed_by IS NULL
                AND billing_observation_reason IS NULL
            )
            OR (
                billing_observation_source = 'provider'
                AND billing_observed_by IS NULL
                AND billing_observation_reason IS NULL
            )
            OR (
                billing_observation_source = 'operator'
                AND billing_observed_by IS NOT NULL
                AND billing_observation_reason IS NOT NULL
            )
        ),
    CONSTRAINT organization_deletions_observation_result_check
        CHECK (
            (
                billing_observation_source IS NULL
                AND last_billing_state IS NULL
                AND billing_checked_at IS NULL
            )
            OR (
                billing_observation_source IS NOT NULL
                AND last_billing_state IS NOT NULL
                AND billing_checked_at IS NOT NULL
            )
        )
);

-- A second non-terminal request must converge on the existing operation rather than create a race.
CREATE UNIQUE INDEX organization_deletions_active_org_idx
    ON organization_deletions (org_id)
    WHERE state NOT IN ('cancelled', 'completed');

CREATE INDEX organization_deletions_org_idx
    ON organization_deletions (org_id, requested_at DESC);

-- Workers find retryable work without scanning terminal history.
CREATE INDEX organization_deletions_retry_idx
    ON organization_deletions (next_attempt_at, id)
    WHERE state IN ('requested', 'cancelling_billing', 'recovering', 'failed');

-- Retention work is due from the fixed purge deadline, not the retry schedule.
CREATE INDEX organization_deletions_retention_idx
    ON organization_deletions (purge_after, id)
    WHERE state = 'retention';
