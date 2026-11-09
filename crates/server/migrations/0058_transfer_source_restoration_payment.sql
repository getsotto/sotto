-- A transfer whose destination payment fails after the source was adjusted stays fenced until
-- the original payer has a replacement paid term. Keep the provider operation and receipt so a
-- webhook retry cannot release the beneficiary slot or charge a second time.
ALTER TABLE billing_transfer_intents
    ADD COLUMN source_restoration_operation_id TEXT,
    ADD COLUMN source_restoration_provider_subscription_id TEXT,
    ADD COLUMN source_restoration_payment_reference TEXT,
    ADD CONSTRAINT billing_transfer_source_restoration_operation_not_empty CHECK (
        source_restoration_operation_id IS NULL OR btrim(source_restoration_operation_id) <> ''
    ),
    ADD CONSTRAINT billing_transfer_source_restoration_subscription_not_empty CHECK (
        source_restoration_provider_subscription_id IS NULL
        OR btrim(source_restoration_provider_subscription_id) <> ''
    ),
    ADD CONSTRAINT billing_transfer_source_restoration_payment_not_empty CHECK (
        source_restoration_payment_reference IS NULL
        OR btrim(source_restoration_payment_reference) <> ''
    ),
    ADD CONSTRAINT billing_transfer_source_restoration_binding_pair CHECK (
        source_restoration_payment_reference IS NULL
        OR (source_restoration_operation_id IS NOT NULL
            AND source_restoration_provider_subscription_id IS NOT NULL)
    );

CREATE UNIQUE INDEX billing_transfer_source_restoration_operation_idx
    ON billing_transfer_intents (source_restoration_operation_id)
    WHERE source_restoration_operation_id IS NOT NULL;

