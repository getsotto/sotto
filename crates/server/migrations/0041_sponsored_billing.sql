-- Durable named sponsor billing. One organisation owns one Stripe subscription; price classes
-- are subscription items and local seats name the beneficiaries allocated against those items.
CREATE TABLE billing_sponsored_subscriptions (
    organization_id TEXT PRIMARY KEY REFERENCES organizations (id) ON DELETE RESTRICT,
    provider_customer_id TEXT UNIQUE,
    provider_subscription_id TEXT UNIQUE,
    provider_schedule_id TEXT UNIQUE,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'active', 'past_due', 'canceled', 'unknown')),
    current_period_start BIGINT,
    current_period_end BIGINT,
    billing_interval TEXT CHECK (billing_interval IS NULL OR billing_interval IN ('month', 'year')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (provider_customer_id IS NULL OR btrim(provider_customer_id) <> ''),
    CHECK (provider_subscription_id IS NULL OR btrim(provider_subscription_id) <> '')
);

CREATE TABLE billing_sponsored_subscription_items (
    subscription_item_id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES billing_sponsored_subscriptions (organization_id) ON DELETE RESTRICT,
    provider_item_id TEXT NOT NULL UNIQUE,
    provider_price_id TEXT NOT NULL,
    offer TEXT NOT NULL CHECK (offer IN ('standard_monthly', 'standard_annual', 'founding_monthly', 'founding_annual')),
    quantity BIGINT NOT NULL CHECK (quantity >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(subscription_item_id) <> ''),
    CHECK (btrim(provider_item_id) <> ''),
    CHECK (btrim(provider_price_id) <> ''),
    UNIQUE (organization_id, offer)
);

-- Durable named sponsor seat operations. Every pending or active row points at an account.
CREATE TABLE billing_sponsored_operations (
    operation_id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    actor_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('add', 'remove', 'replace')),
    beneficiary_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    replacement_beneficiary_id TEXT REFERENCES users (id) ON DELETE RESTRICT,
    offer TEXT NOT NULL CHECK (offer IN ('standard_monthly', 'standard_annual', 'founding_monthly', 'founding_annual')),
    quote_version BIGINT NOT NULL CHECK (quote_version > 0),
    quote_expires_at_epoch BIGINT NOT NULL CHECK (quote_expires_at_epoch > 0),
    effective_from BIGINT NOT NULL CHECK (effective_from >= 0),
    effective_until BIGINT,
    provider_idempotency_key TEXT NOT NULL UNIQUE,
    provider_customer_id TEXT,
    provider_subscription_id TEXT,
    provider_checkout_session_id TEXT,
    provider_payment_reference TEXT,
    provider_item_id TEXT,
    provider_operation_id TEXT,
    provider_checkout_url TEXT,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'checkout_created', 'provider_pending', 'active', 'failed', 'unknown')),
    result_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(operation_id) <> ''),
    CHECK (btrim(idempotency_key) <> ''),
    CHECK (btrim(request_hash) <> ''),
    CHECK (effective_until IS NULL OR effective_until > effective_from),
    CHECK (replacement_beneficiary_id IS NULL OR replacement_beneficiary_id <> beneficiary_id),
    CONSTRAINT sponsored_operation_terminal_result CHECK (
        state IN ('pending', 'checkout_created', 'provider_pending', 'unknown') OR result_code IS NOT NULL
    ),
    UNIQUE (organization_id, actor_user_id, idempotency_key)
);

CREATE TABLE billing_sponsored_seats (
    seat_id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    beneficiary_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    offer TEXT NOT NULL CHECK (offer IN ('standard_monthly', 'standard_annual', 'founding_monthly', 'founding_annual')),
    effective_from BIGINT NOT NULL CHECK (effective_from >= 0),
    effective_until BIGINT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'scheduled_removal', 'replaced', 'canceled')),
    operation_id TEXT NOT NULL UNIQUE REFERENCES billing_sponsored_operations (operation_id),
    provider_item_id TEXT,
    allocation_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(seat_id) <> ''),
    CHECK (effective_until IS NULL OR effective_until > effective_from)
);

CREATE UNIQUE INDEX billing_sponsored_live_beneficiary_idx
    ON billing_sponsored_seats (organization_id, beneficiary_id)
    WHERE state IN ('pending', 'active', 'scheduled_removal');

CREATE INDEX billing_sponsored_seats_org_idx
    ON billing_sponsored_seats (organization_id, state, effective_from);

CREATE INDEX billing_sponsored_operations_recovery_idx
    ON billing_sponsored_operations (state, updated_at)
    WHERE state IN ('pending', 'checkout_created', 'provider_pending', 'unknown');
