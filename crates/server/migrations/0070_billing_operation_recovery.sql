-- Preserve the bounded recovery window for hosted personal Checkouts. Older rows are
-- conservatively backfilled from their original insertion time and remain attributable to the
-- existing personal account/operation binding.
ALTER TABLE billing_operations
    ADD COLUMN reconciliation_attempt_count INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN reconciliation_next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN reconciliation_manual_review_required BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN reconciliation_last_error_code TEXT,
    ADD COLUMN provider_checkout_session_id TEXT,
    ADD COLUMN provider_checkout_price_id TEXT,
    ADD COLUMN provider_checkout_success_url TEXT,
    ADD COLUMN provider_checkout_cancel_url TEXT;

-- A provider-confirmed expired Checkout has no Stripe subscription to retain. Canceled accounts
-- are already reusable, so permit that terminal state without weakening paid-state binding.
ALTER TABLE billing_personal_accounts
    DROP CONSTRAINT billing_personal_accounts_check,
    ADD CONSTRAINT billing_personal_accounts_subscription_required CHECK (
        state IN ('pending', 'canceled') OR stripe_subscription_id IS NOT NULL
    );

ALTER TABLE billing_operations
    DROP CONSTRAINT billing_operations_state_valid,
    ADD CONSTRAINT billing_operations_state_valid
        CHECK (state IN ('pending', 'succeeded', 'failed', 'unknown', 'manual_review')),
    DROP CONSTRAINT billing_operations_result_requires_terminal,
    ADD CONSTRAINT billing_operations_result_requires_terminal CHECK (
        state IN ('pending', 'unknown') OR result_code IS NOT NULL
    ),
    ADD CONSTRAINT billing_operations_recovery_attempts_nonnegative
        CHECK (reconciliation_attempt_count >= 0),
    ADD CONSTRAINT billing_operations_recovery_error_code_bounded
        CHECK (reconciliation_last_error_code IS NULL OR
               (length(reconciliation_last_error_code) BETWEEN 1 AND 64 AND
                reconciliation_last_error_code ~ '^[a-z0-9_]+$')),
    ADD CONSTRAINT billing_operations_recovery_manual_state CHECK (
        NOT reconciliation_manual_review_required OR state = 'manual_review'
    ),
    ADD CONSTRAINT billing_operations_checkout_request_complete CHECK (
        (provider_checkout_price_id IS NULL AND provider_checkout_success_url IS NULL
            AND provider_checkout_cancel_url IS NULL)
        OR (provider_checkout_price_id IS NOT NULL AND btrim(provider_checkout_price_id) <> ''
            AND provider_checkout_success_url IS NOT NULL AND btrim(provider_checkout_success_url) <> ''
            AND provider_checkout_cancel_url IS NOT NULL AND btrim(provider_checkout_cancel_url) <> '')
    );

CREATE INDEX billing_operations_recovery_due_idx
    ON billing_operations (reconciliation_next_attempt_at, updated_at, operation_id)
    WHERE state IN ('pending', 'unknown') AND NOT reconciliation_manual_review_required;

CREATE INDEX billing_operations_checkout_session_idx
    ON billing_operations (provider_checkout_session_id)
    WHERE provider_checkout_session_id IS NOT NULL;
