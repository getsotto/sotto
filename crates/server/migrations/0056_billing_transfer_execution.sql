-- Provider work must survive process restarts without creating a second financial operation.
-- Lease fields fence workers; provider references identify the result of each external step.
ALTER TABLE billing_transfer_intents
    ADD COLUMN execution_lease_token TEXT,
    ADD COLUMN execution_lease_until TIMESTAMPTZ,
    ADD COLUMN execution_next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN execution_attempt_count INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN destination_customer_id TEXT,
    ADD COLUMN destination_schedule_id TEXT,
    ADD COLUMN source_schedule_id TEXT,
    ADD COLUMN destination_setup_session_id TEXT,
    ADD COLUMN destination_setup_intent_id TEXT,
    ADD COLUMN last_execution_error_code TEXT,
    ADD COLUMN execution_manual_review_required BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN source_restoration_reference TEXT;

-- Once the source schedule has been changed, a failed destination payment must remain live
-- until a provider read-back confirms the source was restored. This state is recoverable and
-- therefore belongs in both the live-beneficiary fence and the worker's due set.
ALTER TABLE billing_transfer_intents
    DROP CONSTRAINT billing_transfer_intents_state_check,
    DROP CONSTRAINT billing_transfer_intents_check4,
    ADD CONSTRAINT billing_transfer_intents_state_check CHECK (
        state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                  'source_adjustment_pending', 'source_restoration_pending',
                  'completed', 'failed', 'unknown')
    ),
    ADD CONSTRAINT billing_transfer_intents_check4 CHECK (
        state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                  'source_adjustment_pending', 'source_restoration_pending', 'unknown')
        OR result_code IS NOT NULL
    ),
    ADD CONSTRAINT billing_transfer_source_restoration_reference_not_empty CHECK (
        source_restoration_reference IS NULL OR btrim(source_restoration_reference) <> ''
    );

DROP INDEX billing_transfer_live_beneficiary_idx;
CREATE UNIQUE INDEX billing_transfer_live_beneficiary_idx
    ON billing_transfer_intents (beneficiary_id)
    WHERE state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                    'source_adjustment_pending', 'source_restoration_pending', 'unknown');

DROP INDEX billing_transfer_recovery_idx;
CREATE INDEX billing_transfer_recovery_idx
    ON billing_transfer_intents (state, updated_at)
    WHERE state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                    'source_adjustment_pending', 'source_restoration_pending', 'unknown');

ALTER TABLE billing_transfer_intents
    ADD CONSTRAINT billing_transfer_execution_lease_pair CHECK (
        (execution_lease_token IS NULL AND execution_lease_until IS NULL)
        OR (execution_lease_token IS NOT NULL AND execution_lease_until IS NOT NULL)
    ),
    ADD CONSTRAINT billing_transfer_execution_attempts_valid CHECK (
        execution_attempt_count >= 0
    ),
    ADD CONSTRAINT billing_transfer_execution_lease_token_not_empty CHECK (
        execution_lease_token IS NULL OR btrim(execution_lease_token) <> ''
    );

-- Older transfer intents predate source subscription binding. They have never had provider work
-- attached (the transfer API was ships-dark), so do not let an unbound invitation become
-- executable after the provider worker is introduced. Keep the rows for history and idempotent
-- replies, but release their beneficiary slot with an explicit reason.
UPDATE billing_transfer_intents
SET state = 'failed', result_code = 'source_binding_unavailable', updated_at = now()
WHERE source_provider_subscription_id IS NULL
  AND state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                'source_adjustment_pending', 'source_restoration_pending', 'unknown');

CREATE INDEX billing_transfer_execution_due_idx
    ON billing_transfer_intents (execution_next_attempt_at, created_at, transfer_id)
    WHERE state IN ('pending', 'destination_prepared', 'destination_paid',
                    'source_adjustment_pending', 'source_restoration_pending', 'unknown');
