-- The first 100 founding places are person-owned awards. Reservations are temporary capacity
-- claims and are never themselves evidence of payment.
CREATE TABLE billing_founding_capacity (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    max_places BIGINT NOT NULL DEFAULT 100 CHECK (max_places = 100),
    CHECK (singleton)
);

INSERT INTO billing_founding_capacity (singleton) VALUES (TRUE);

CREATE TABLE billing_founding_reservations (
    reservation_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL UNIQUE,
    beneficiary_id TEXT NOT NULL,
    payer_id TEXT NOT NULL,
    offer TEXT NOT NULL CHECK (offer IN ('founding_monthly', 'founding_annual')),
    quote_version BIGINT NOT NULL CHECK (quote_version > 0),
    quote_expires_at_epoch BIGINT NOT NULL CHECK (quote_expires_at_epoch > 0),
    status TEXT NOT NULL DEFAULT 'reserved'
        CHECK (status IN ('reserved', 'confirmed', 'refund_required')),
    award_id TEXT,
    payment_reference TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(reservation_id) <> ''),
    CHECK (btrim(operation_id) <> ''),
    CHECK (btrim(beneficiary_id) <> ''),
    CHECK (btrim(payer_id) <> ''),
    CHECK ((status = 'confirmed') = (award_id IS NOT NULL AND payment_reference IS NOT NULL))
);

CREATE INDEX billing_founding_reservations_beneficiary_idx
    ON billing_founding_reservations (beneficiary_id, status);

CREATE INDEX billing_founding_reservations_expiry_idx
    ON billing_founding_reservations (quote_expires_at_epoch)
    WHERE status = 'reserved';

CREATE TABLE billing_founding_awards (
    award_id TEXT PRIMARY KEY,
    beneficiary_id TEXT NOT NULL UNIQUE,
    payer_id TEXT NOT NULL,
    offer TEXT NOT NULL CHECK (offer IN ('founding_monthly', 'founding_annual')),
    cohort_ordinal BIGINT NOT NULL UNIQUE CHECK (cohort_ordinal BETWEEN 1 AND 100),
    original_start_date TEXT NOT NULL,
    original_end_date TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(award_id) <> ''),
    CHECK (btrim(beneficiary_id) <> ''),
    CHECK (btrim(payer_id) <> '')
);

CREATE INDEX billing_founding_awards_payer_idx
    ON billing_founding_awards (payer_id);
