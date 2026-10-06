-- Durable refund/correction requests. A request alone never changes a paid term; the term can
-- end early only after a provider-confirmed full refund and an explicit customer confirmation.
CREATE TABLE billing_correction_requests (
    request_id TEXT PRIMARY KEY,
    requester_user_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    beneficiary_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    organization_id TEXT REFERENCES organizations (id) ON DELETE RESTRICT,
    payer_kind TEXT NOT NULL CHECK (payer_kind IN ('personal', 'sponsor')),
    payment_reference TEXT NOT NULL,
    subscription_id TEXT NOT NULL,
    amount_pence BIGINT,
    reason TEXT NOT NULL CHECK (reason IN ('duplicate_charge', 'billing_error', 'accidental_renewal', 'legal_requirement')),
    policy_version TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    full_refund_requested BOOLEAN NOT NULL DEFAULT FALSE,
    state TEXT NOT NULL DEFAULT 'requested'
        CHECK (state IN ('requested', 'approved', 'provider_pending', 'termination_pending', 'refunded', 'denied', 'failed', 'unknown')),
    preserve_paid_term BOOLEAN NOT NULL DEFAULT TRUE,
    early_termination_confirmed_at_epoch BIGINT,
    effective_at_epoch BIGINT,
    provider_refund_id TEXT,
    result_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(request_id) <> ''),
    CHECK (btrim(payment_reference) <> ''),
    CHECK (btrim(subscription_id) <> ''),
    CHECK (btrim(policy_version) <> ''),
    CHECK (btrim(idempotency_key) <> ''),
    CHECK (btrim(request_hash) <> ''),
    CHECK (amount_pence IS NULL OR amount_pence > 0),
    CHECK (NOT full_refund_requested OR amount_pence IS NULL),
    CHECK ((payer_kind = 'personal') = (organization_id IS NULL)),
    CHECK ((early_termination_confirmed_at_epoch IS NULL) = (effective_at_epoch IS NULL)),
    CHECK (state IN ('requested', 'approved', 'provider_pending', 'unknown') OR result_code IS NOT NULL),
    UNIQUE (requester_user_id, idempotency_key),
    UNIQUE (provider_refund_id)
);

CREATE INDEX billing_correction_requests_state_idx
    ON billing_correction_requests (state, updated_at);
