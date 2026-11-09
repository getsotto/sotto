-- Keep the period Stripe read back while preparing the destination. A mismatch on the eventual
-- paid invoice is retained for operator review without aborting that invoice's billing ingest.
ALTER TABLE billing_transfer_intents
    ADD COLUMN destination_expected_period_end BIGINT,
    ADD CONSTRAINT billing_transfer_expected_period_end_valid CHECK (
        destination_expected_period_end IS NULL
        OR destination_expected_period_end > effective_from
    );
