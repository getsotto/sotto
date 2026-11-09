-- A quote is short-lived, but a consent invitation needs time to reach the other payer.
-- Preserve the old deadline for any row that may already be awaiting consent.
ALTER TABLE billing_transfer_intents
    ADD COLUMN consent_expires_at_epoch BIGINT;

UPDATE billing_transfer_intents
SET consent_expires_at_epoch = quote_expires_at_epoch;

ALTER TABLE billing_transfer_intents
    ALTER COLUMN consent_expires_at_epoch SET NOT NULL,
    ADD CONSTRAINT billing_transfer_consent_expiry_valid
        CHECK (consent_expires_at_epoch > 0);

CREATE INDEX billing_transfer_consent_expiry_idx
    ON billing_transfer_intents (consent_expires_at_epoch, beneficiary_id)
    WHERE state = 'awaiting_consent';
