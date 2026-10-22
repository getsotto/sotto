ALTER TABLE billing_sponsored_operations
    ADD COLUMN provider_request_version SMALLINT,
    ADD COLUMN provider_request JSONB,
    ADD COLUMN provider_first_sent_at TIMESTAMPTZ,
    ADD COLUMN recovery_attempt_count INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN recovery_next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN recovery_lease_token TEXT,
    ADD COLUMN recovery_lease_until TIMESTAMPTZ,
    ADD COLUMN recovery_manual_review_required BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN recovery_last_error_code TEXT;

ALTER TABLE billing_sponsored_operations
    DROP CONSTRAINT sponsored_operation_terminal_result,
    ADD CONSTRAINT sponsored_operation_terminal_result CHECK (
        state IN ('pending', 'checkout_created', 'provider_pending', 'unknown')
        OR result_code IS NOT NULL
    ),
    DROP CONSTRAINT billing_sponsored_operations_state_check,
    ADD CONSTRAINT billing_sponsored_operations_state_check CHECK (
        state IN ('pending', 'checkout_created', 'provider_pending', 'active', 'failed', 'unknown', 'manual_review')
    ),
    ADD CONSTRAINT sponsored_operation_recovery_attempts_nonnegative
        CHECK (recovery_attempt_count >= 0),
    ADD CONSTRAINT sponsored_operation_recovery_request_pair CHECK (
        (provider_request_version IS NULL AND provider_request IS NULL)
        OR (provider_request_version = 1 AND provider_request IS NOT NULL
            AND jsonb_typeof(provider_request) = 'object')
    ),
    ADD CONSTRAINT sponsored_operation_recovery_lease_pair CHECK (
        (recovery_lease_token IS NULL AND recovery_lease_until IS NULL)
        OR (recovery_lease_token IS NOT NULL AND btrim(recovery_lease_token) <> ''
            AND recovery_lease_until IS NOT NULL)
    ),
    ADD CONSTRAINT sponsored_operation_recovery_error_code_bounded CHECK (
        recovery_last_error_code IS NULL OR
        (length(recovery_last_error_code) BETWEEN 1 AND 64
            AND recovery_last_error_code ~ '^[a-z0-9_]+$')
    ),
    ADD CONSTRAINT sponsored_operation_recovery_manual_state CHECK (
        NOT recovery_manual_review_required OR state = 'manual_review'
    );

CREATE INDEX billing_sponsored_operations_recovery_due_idx
    ON billing_sponsored_operations (recovery_next_attempt_at, updated_at, operation_id)
    WHERE state IN ('pending', 'unknown', 'checkout_created')
      AND NOT recovery_manual_review_required
      AND provider_request_version = 1
      AND provider_request IS NOT NULL;

-- Pending writes from earlier releases have no immutable request to replay. Keep their identities
-- visible for operator inspection rather than rebuilding a potentially different provider call.
UPDATE billing_sponsored_operations
SET state = 'manual_review', result_code = 'request_facts_missing',
    recovery_manual_review_required = TRUE, recovery_last_error_code = 'request_facts_missing',
    updated_at = now()
WHERE state IN ('pending','unknown','checkout_created') AND provider_request IS NULL;
