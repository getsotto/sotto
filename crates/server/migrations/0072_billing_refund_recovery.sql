-- Persist the first refund-create attempt so retries stop before Stripe's idempotency
-- records can expire. Older ambiguous requests use their creation time conservatively.
ALTER TABLE billing_correction_requests
    ADD COLUMN provider_refund_started_at_epoch BIGINT;

UPDATE billing_correction_requests
SET provider_refund_started_at_epoch = FLOOR(EXTRACT(EPOCH FROM created_at))::BIGINT
WHERE state = 'provider_pending'
  AND provider_refund_id IS NULL;

ALTER TABLE billing_correction_requests
    ADD CONSTRAINT billing_correction_requests_refund_started_nonnegative
    CHECK (provider_refund_started_at_epoch IS NULL OR provider_refund_started_at_epoch >= 0);

-- A database claim serialises refund creation with horizon parking without holding a connection
-- while Stripe is called. The token lets the caller recheck that its claim still owns the send.
ALTER TABLE billing_correction_requests
    ADD COLUMN provider_refund_claim_token UUID,
    ADD COLUMN provider_refund_claimed_until TIMESTAMPTZ,
    ADD CONSTRAINT billing_correction_requests_refund_claim_pair
        CHECK ((provider_refund_claim_token IS NULL) = (provider_refund_claimed_until IS NULL));
