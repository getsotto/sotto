-- A payer transfer is a durable, beneficiary-scoped intent. Provider calls happen outside the
-- transaction and every callback records evidence against this row before the next step runs.
ALTER TABLE billing_founding_reservations
    ADD COLUMN payer_kind TEXT NOT NULL DEFAULT 'personal'
        CHECK (payer_kind IN ('personal', 'sponsor'));

ALTER TABLE billing_founding_awards
    ADD COLUMN payer_kind TEXT NOT NULL DEFAULT 'personal'
        CHECK (payer_kind IN ('personal', 'sponsor'));

CREATE TABLE billing_transfer_intents (
    transfer_id TEXT PRIMARY KEY,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    counterparty_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    beneficiary_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('personal', 'sponsor')),
    source_organization_id TEXT REFERENCES organizations (id) ON DELETE RESTRICT,
    destination_kind TEXT NOT NULL CHECK (destination_kind IN ('personal', 'sponsor')),
    destination_organization_id TEXT REFERENCES organizations (id) ON DELETE RESTRICT,
    offer TEXT NOT NULL CHECK (offer IN ('standard_monthly', 'standard_annual', 'founding_monthly', 'founding_annual')),
    quote_version BIGINT NOT NULL CHECK (quote_version > 0),
    quote_expires_at_epoch BIGINT NOT NULL CHECK (quote_expires_at_epoch > 0),
    effective_from BIGINT NOT NULL CHECK (effective_from >= 0),
    effective_until BIGINT,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    provider_idempotency_key TEXT NOT NULL UNIQUE,
    destination_operation_id TEXT,
    source_operation_id TEXT,
    destination_provider_subscription_id TEXT,
    source_provider_subscription_id TEXT,
    destination_payment_reference TEXT,
    source_adjustment_reference TEXT,
    founding_award_id TEXT,
    state TEXT NOT NULL DEFAULT 'awaiting_consent'
        CHECK (state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid',
                         'source_adjustment_pending', 'completed', 'failed', 'unknown')),
    result_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(transfer_id) <> ''),
    CHECK (btrim(counterparty_user_id) <> ''),
    CHECK (btrim(idempotency_key) <> ''),
    CHECK (btrim(request_hash) <> ''),
    CHECK (effective_until IS NULL OR effective_until > effective_from),
    CHECK ((source_kind = 'personal') = (source_organization_id IS NULL)),
    CHECK ((destination_kind = 'personal') = (destination_organization_id IS NULL)),
    CHECK (source_kind <> destination_kind OR source_organization_id IS DISTINCT FROM destination_organization_id),
    CHECK (state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid', 'source_adjustment_pending', 'unknown')
           OR result_code IS NOT NULL),
    UNIQUE (actor_user_id, idempotency_key)
);

CREATE UNIQUE INDEX billing_transfer_live_beneficiary_idx
    ON billing_transfer_intents (beneficiary_id)
    WHERE state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid', 'source_adjustment_pending', 'unknown');

CREATE INDEX billing_transfer_recovery_idx
    ON billing_transfer_intents (state, updated_at)
    WHERE state IN ('awaiting_consent', 'pending', 'destination_prepared', 'destination_paid', 'source_adjustment_pending', 'unknown');
