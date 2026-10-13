-- Keep scheduled sponsored additions reserved until their renewal can settle. A founding Checkout
-- that completes before its own expiry also records that completion and retains the place while a
-- delayed payment method is being processed.
ALTER TABLE billing_founding_reservations
    ADD COLUMN reservation_expires_at_epoch BIGINT,
    ADD COLUMN checkout_completed_by_epoch BIGINT;

UPDATE billing_founding_reservations
SET reservation_expires_at_epoch = quote_expires_at_epoch;

ALTER TABLE billing_founding_reservations
    ALTER COLUMN reservation_expires_at_epoch SET NOT NULL,
    ADD CONSTRAINT billing_founding_reservation_deadline_check
        CHECK (reservation_expires_at_epoch >= quote_expires_at_epoch),
    DROP CONSTRAINT billing_founding_reservations_status_check,
    ADD CONSTRAINT billing_founding_reservations_status_check
        CHECK (status IN ('reserved', 'confirmed', 'refund_required', 'released'));

CREATE INDEX billing_founding_reservations_deadline_idx
    ON billing_founding_reservations (reservation_expires_at_epoch)
    WHERE status = 'reserved';
