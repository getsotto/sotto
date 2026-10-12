-- Personal Cloud billing is separate from organisation entitlements. A row is created only for a
-- durable checkout operation and becomes active after a verified paid settlement.
ALTER TABLE billing_operations
    ADD COLUMN provider_checkout_url TEXT;

CREATE TABLE billing_personal_accounts (
    user_id                  TEXT PRIMARY KEY REFERENCES users (id) ON DELETE RESTRICT,
    operation_id             TEXT NOT NULL UNIQUE REFERENCES billing_operations (operation_id),
    offer                    TEXT NOT NULL CHECK (
        offer IN ('standard_monthly', 'standard_annual', 'founding_monthly', 'founding_annual')
    ),
    stripe_customer_id       TEXT UNIQUE,
    stripe_subscription_id   TEXT UNIQUE,
    state                    TEXT NOT NULL DEFAULT 'pending' CHECK (
        state IN ('pending', 'active', 'past_due', 'unpaid', 'canceled', 'refund_required')
    ),
    pending_expires_at_epoch BIGINT NOT NULL,
    paid_through_epoch       BIGINT,
    cancel_at_period_end     BOOLEAN NOT NULL DEFAULT FALSE,
    cancellation_requested_at TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(user_id) <> ''),
    CHECK (pending_expires_at_epoch > 0),
    CHECK (paid_through_epoch IS NULL OR paid_through_epoch > 0),
    CHECK ((state = 'pending') OR stripe_subscription_id IS NOT NULL)
);

CREATE INDEX billing_personal_accounts_state_idx
    ON billing_personal_accounts (state, updated_at);
