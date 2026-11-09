-- A grouped sponsor invoice can fail for several named allocations in one event. Keep one
-- immutable evidence row per beneficiary/allocation while the event receipt and invalidation
-- association remain beneficiary-scoped.
ALTER TABLE cloud_provider_stripe_renewal_failures
    DROP CONSTRAINT cloud_provider_stripe_renewal_failures_pkey;

ALTER TABLE cloud_provider_stripe_renewal_failures
    ADD CONSTRAINT cloud_provider_stripe_renewal_failures_pkey
        PRIMARY KEY (provider_namespace, provider_account_id, provider_environment,
                     event_id, beneficiary_id, allocation_id);

ALTER TABLE cloud_provider_stripe_renewal_failures
    ADD COLUMN provider_price_id TEXT
        CHECK (provider_price_id IS NULL OR btrim(provider_price_id) <> '');

ALTER TABLE cloud_provider_stripe_renewal_failures
    ADD COLUMN evidence_provenance TEXT NOT NULL DEFAULT 'legacy'
        CHECK (evidence_provenance IN ('legacy', 'signed_webhook', 'authenticated_api_read'));

CREATE INDEX cloud_provider_stripe_renewal_failures_subscription_invoice_idx
    ON cloud_provider_stripe_renewal_failures
        (provider_namespace, provider_account_id, provider_environment,
         subscription_id, invoice_id, event_id, beneficiary_id, allocation_id)
    WHERE provider_price_id IS NOT NULL;

CREATE TABLE cloud_provider_stripe_renewal_resolutions (
    provider_namespace       TEXT NOT NULL CHECK (provider_namespace = 'stripe'),
    provider_account_id      TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment     TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id                 TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    beneficiary_id           TEXT NOT NULL CHECK (btrim(beneficiary_id) <> ''),
    allocation_id            TEXT NOT NULL REFERENCES cloud_provider_allocations (allocation_id)
                             ON DELETE RESTRICT,
    resolution_invoice_id    TEXT NOT NULL CHECK (btrim(resolution_invoice_id) <> ''),
    resolved_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_namespace, provider_account_id, provider_environment,
                 event_id, beneficiary_id, allocation_id),
    FOREIGN KEY (provider_namespace, provider_account_id, provider_environment,
                 event_id, beneficiary_id, allocation_id)
        REFERENCES cloud_provider_stripe_renewal_failures
            (provider_namespace, provider_account_id, provider_environment,
             event_id, beneficiary_id, allocation_id)
        ON DELETE RESTRICT
);

CREATE INDEX cloud_provider_stripe_renewal_resolutions_invoice_idx
    ON cloud_provider_stripe_renewal_resolutions
        (provider_namespace, provider_account_id, provider_environment, resolution_invoice_id);
