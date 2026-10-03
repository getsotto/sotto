-- Durable billing intent identity. The row is created before provider I/O so a timeout or
-- process restart can reconcile the same financial operation instead of creating a new one.
CREATE TABLE billing_operations (
    operation_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    actor_user_id TEXT NOT NULL,
    payer_id TEXT NOT NULL,
    beneficiary_id TEXT NOT NULL,
    offer TEXT NOT NULL,
    quote_version BIGINT NOT NULL,
    quote_expires_at_epoch BIGINT NOT NULL,
    provider_idempotency_key TEXT NOT NULL UNIQUE,
    provider_operation_id TEXT,
    reconciliation_lease_token TEXT,
    reconciliation_lease_until TIMESTAMPTZ,
    state TEXT NOT NULL DEFAULT 'pending',
    result_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT billing_operations_idempotency_key_not_empty CHECK (btrim(idempotency_key) <> ''),
    CONSTRAINT billing_operations_request_hash_not_empty CHECK (btrim(request_hash) <> ''),
    CONSTRAINT billing_operations_actor_not_empty CHECK (btrim(actor_user_id) <> ''),
    CONSTRAINT billing_operations_payer_not_empty CHECK (btrim(payer_id) <> ''),
    CONSTRAINT billing_operations_beneficiary_not_empty CHECK (btrim(beneficiary_id) <> ''),
    CONSTRAINT billing_operations_offer_not_empty CHECK (btrim(offer) <> ''),
    CONSTRAINT billing_operations_provider_key_not_empty CHECK (btrim(provider_idempotency_key) <> ''),
    CONSTRAINT billing_operations_reconciliation_lease_pair CHECK (
        (reconciliation_lease_token IS NULL AND reconciliation_lease_until IS NULL)
        OR (reconciliation_lease_token IS NOT NULL AND reconciliation_lease_until IS NOT NULL)
    ),
    CONSTRAINT billing_operations_quote_version_positive CHECK (quote_version > 0),
    CONSTRAINT billing_operations_quote_expiry_positive CHECK (quote_expires_at_epoch > 0),
    CONSTRAINT billing_operations_state_valid CHECK (state IN ('pending', 'succeeded', 'failed', 'unknown')),
    CONSTRAINT billing_operations_result_requires_terminal CHECK (
        state IN ('pending', 'unknown') OR result_code IS NOT NULL
    )
);

CREATE UNIQUE INDEX billing_operations_actor_idempotency_idx
    ON billing_operations (actor_user_id, idempotency_key);

CREATE INDEX billing_operations_reconciliation_idx
    ON billing_operations (state, updated_at)
    WHERE state IN ('pending', 'unknown');
