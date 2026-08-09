-- Organisation deletion lifecycle data. The lifecycle flag is kept on the hot organisation row;
-- the operation table stores the durable workflow, lease, billing, and recovery history.
--
-- The retained organisation row is a tombstone, not a second source of workflow state. Its
-- constraints make the deleted shape enforceable even when a stale webhook or operator query
-- reaches the database directly. The operation checks below keep worker compare-and-set updates,
-- provider observations, and recovery evidence internally consistent.

ALTER TABLE organizations
    -- Active and deleting rows remain fully addressable; deleted rows retain only their identity.
    ADD COLUMN lifecycle_state TEXT NOT NULL DEFAULT 'active',
    ADD COLUMN deleted_at TIMESTAMPTZ,
    -- A timestamp is present exactly while the row is a deleted tombstone.
    ADD CONSTRAINT organizations_lifecycle_state_check
        CHECK (lifecycle_state IN ('active', 'deleting', 'deleted')),
    ADD CONSTRAINT organizations_lifecycle_deleted_at_check
        CHECK (
            (lifecycle_state = 'deleted' AND deleted_at IS NOT NULL)
            OR (lifecycle_state <> 'deleted' AND deleted_at IS NULL)
        ),
    -- The encrypted name is cleared in the same statement that marks the row deleted.
    ADD CONSTRAINT organizations_lifecycle_enc_name_check
        CHECK (
            (lifecycle_state = 'deleted' AND enc_name IS NULL)
            OR (lifecycle_state <> 'deleted' AND enc_name IS NOT NULL)
        ),
    -- Billing, trial, and creator data must not survive the final purge or a late webhook.
    ADD CONSTRAINT organizations_lifecycle_tombstone_check
        CHECK (
            lifecycle_state <> 'deleted'
            OR (
                created_by IS NULL
                AND tier = 'free'
                AND trial_ends_at IS NULL
                AND stripe_customer_id IS NULL
                AND stripe_subscription_id IS NULL
            )
        );

-- A deleted organisation is a tombstone, so its encrypted name is cleared at final purge.
ALTER TABLE organizations
    ALTER COLUMN enc_name DROP NOT NULL;

CREATE TABLE organization_deletions (
    id UUID PRIMARY KEY,
    -- RESTRICT preserves the organisation tombstone and its operation history as one unit.
    org_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    -- State is the worker's durable phase; resume_state records where a failed attempt restarts.
    -- The explicit list makes an unknown workflow phase fail closed at the database boundary.
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
    -- A failed attempt may resume only from a non-terminal worker phase.
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
    -- These values are the provider result used by the final purge gate, never entitlement alone.
    last_billing_state TEXT
        CHECK (last_billing_state IS NULL OR last_billing_state IN (
            'blocking', 'terminal', 'missing', 'unknown'
        )),
    billing_checked_at TIMESTAMPTZ,
    -- Provider and operator sources have different accountability requirements below.
    billing_observation_source TEXT
        CHECK (billing_observation_source IS NULL OR billing_observation_source IN (
            'provider', 'operator'
        )),
    billing_observed_by TEXT,
    billing_observation_reason TEXT,
    billing_observation_evidence TEXT,
    -- Retries are bounded by attempt_count and scheduled through next_attempt_at.
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    next_attempt_at TIMESTAMPTZ,
    last_error_code TEXT,
    -- Workers increment state_version so a late result cannot overwrite a newer transition.
    state_version BIGINT NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    -- A lease owner and expiry are paired so a worker can always be reclaimed after a crash.
    lease_owner TEXT,
    lease_expires_at TIMESTAMPTZ,
    -- Terminal timestamps make completion and cancellation auditable and state-consistent.
    cancelled_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    managed_backup_expiry_by TIMESTAMPTZ,
    -- Retention must not be shortened accidentally, and all derived deadlines follow the request.
    CONSTRAINT organization_deletions_purge_after_check
        CHECK (purge_after >= requested_at),
    CONSTRAINT organization_deletions_failed_resume_state_check
        CHECK ((state = 'failed') = (resume_state IS NOT NULL)),
    -- A billing state is meaningful only with the time at which it was observed.
    CONSTRAINT organization_deletions_billing_result_pair_check
        CHECK ((last_billing_state IS NULL) = (billing_checked_at IS NULL)),
    -- An observation cannot predate the deletion request it is meant to gate.
    CONSTRAINT organization_deletions_billing_checked_at_check
        CHECK (billing_checked_at IS NULL OR billing_checked_at >= requested_at),
    -- Leases are all-or-nothing and must not expire before the operation exists.
    CONSTRAINT organization_deletions_lease_pair_check
        CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT organization_deletions_lease_expires_at_check
        CHECK (lease_expires_at IS NULL OR lease_expires_at >= requested_at),
    -- Retry scheduling is relative to the original request, never before it.
    CONSTRAINT organization_deletions_next_attempt_at_check
        CHECK (next_attempt_at IS NULL OR next_attempt_at >= requested_at),
    -- Managed backups must outlive the retention purge deadline.
    CONSTRAINT organization_deletions_backup_expiry_check
        CHECK (managed_backup_expiry_by IS NULL OR managed_backup_expiry_by >= purge_after),
    -- Cancellation and completion timestamps are present only for their matching terminal state.
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
    -- Provider observations have no actor; operator observations require accountable evidence.
    CONSTRAINT organization_deletions_observation_actor_check
        CHECK (
            (
                billing_observation_source IS NULL
                AND billing_observed_by IS NULL
                AND billing_observation_reason IS NULL
                AND billing_observation_evidence IS NULL
            )
            OR (
                billing_observation_source = 'provider'
                AND billing_observed_by IS NULL
                AND billing_observation_reason IS NULL
                AND billing_observation_evidence IS NULL
            )
            OR (
                billing_observation_source = 'operator'
                AND billing_observed_by IS NOT NULL
                AND billing_observation_reason IS NOT NULL
                AND billing_observation_evidence IS NOT NULL
            )
        ),
    -- Source, state, and timestamp are stored together so no partial observation can satisfy the
    -- purge gate.
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

-- Workers reclaim expired leases, including purging work left behind by a crashed worker.
CREATE INDEX organization_deletions_lease_idx
    ON organization_deletions (lease_expires_at, id)
    WHERE lease_expires_at IS NOT NULL;

-- Retention work is due from the fixed purge deadline, not the retry schedule.
CREATE INDEX organization_deletions_retention_idx
    ON organization_deletions (purge_after, id)
    WHERE state = 'retention';
