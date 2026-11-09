-- Keep the exact paid destination term with the payment reference. Webhook replays must not
-- change the period that justified moving the beneficiary to the destination payer.
ALTER TABLE billing_transfer_intents
    ADD COLUMN destination_period_end BIGINT,
    ADD CONSTRAINT billing_transfer_destination_period_end_valid CHECK (
        destination_period_end IS NULL OR destination_period_end > effective_from
    ),
    ADD CONSTRAINT billing_transfer_destination_period_has_payment CHECK (
        destination_period_end IS NULL OR destination_payment_reference IS NOT NULL
    );
