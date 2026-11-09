-- Manual review must be an explicit, attributable decision. A generation invalidates an
-- approval whenever new evidence parks the transfer again.
ALTER TABLE billing_transfer_intents
    ADD COLUMN execution_review_generation BIGINT NOT NULL DEFAULT 0,
    ADD CONSTRAINT billing_transfer_review_generation_valid
        CHECK (execution_review_generation >= 0);

-- Keep the exact destination invoice period that was accepted or parked for review. A resumed
-- transfer must be able to re-check the same evidence before it retires the source payer.
ALTER TABLE billing_transfer_intents
    DROP CONSTRAINT billing_transfer_destination_period_end_valid,
    ADD COLUMN destination_period_start BIGINT,
    ADD CONSTRAINT billing_transfer_destination_period_start_valid CHECK (
        destination_period_start IS NULL OR destination_period_start > 0
    ),
    ADD CONSTRAINT billing_transfer_destination_period_end_valid CHECK (
        destination_period_end IS NULL OR destination_period_end > 0
    ),
    ADD CONSTRAINT billing_transfer_destination_period_order_valid CHECK (
        destination_period_start IS NULL OR destination_period_end IS NULL
        OR destination_period_end > destination_period_start
    );

CREATE TABLE billing_transfer_review_decisions (
    decision_id BIGSERIAL PRIMARY KEY,
    transfer_id TEXT NOT NULL REFERENCES billing_transfer_intents(transfer_id) ON DELETE RESTRICT,
    operator_label TEXT NOT NULL CHECK (
        length(btrim(operator_label)) BETWEEN 1 AND 128
        AND operator_label !~ '[[:cntrl:]]'
    ),
    action TEXT NOT NULL CHECK (action IN ('hold', 'resume')),
    reason_code TEXT NOT NULL CHECK (
        reason_code ~ '^[a-z][a-z0-9_]{0,63}$'
    ),
    review_generation BIGINT NOT NULL CHECK (review_generation >= 0),
    idempotency_key TEXT NOT NULL CHECK (
        length(btrim(idempotency_key)) BETWEEN 1 AND 128
        AND idempotency_key !~ '[[:cntrl:]]'
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (transfer_id, idempotency_key)
);

CREATE INDEX billing_transfer_review_decisions_history_idx
    ON billing_transfer_review_decisions (transfer_id, decision_id DESC);

CREATE INDEX billing_transfer_manual_review_queue_idx
    ON billing_transfer_intents (transfer_id)
    WHERE execution_manual_review_required
      AND state IN ('destination_prepared', 'source_adjustment_pending', 'unknown');
