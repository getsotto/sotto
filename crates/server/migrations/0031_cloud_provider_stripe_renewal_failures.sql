-- Persist a fully linked, signature-verified personal renewal failure.
-- The generic receipt and invalidation association remain the source of event and fence identity;
-- this immutable row stores the Stripe-specific predecessor provenance needed by later reads.

CREATE TABLE cloud_provider_stripe_renewal_failures (
    provider_namespace              TEXT NOT NULL CHECK (provider_namespace = 'stripe'),
    provider_account_id             TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment            TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id                        TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    evidence_version                SMALLINT NOT NULL CHECK (evidence_version > 0),
    evidence_reference              TEXT NOT NULL CHECK (btrim(evidence_reference) <> ''),
    renewal_id                      TEXT NOT NULL CHECK (btrim(renewal_id) <> ''),
    invoice_id                      TEXT NOT NULL CHECK (btrim(invoice_id) <> ''),
    invoice_line_id                 TEXT NOT NULL CHECK (btrim(invoice_line_id) <> ''),
    predecessor_invoice_id          TEXT NOT NULL CHECK (btrim(predecessor_invoice_id) <> ''),
    predecessor_evidence_reference  TEXT NOT NULL
                                     CHECK (btrim(predecessor_evidence_reference) <> ''),
    provider_customer_id            TEXT NOT NULL CHECK (btrim(provider_customer_id) <> ''),
    subscription_id                 TEXT NOT NULL CHECK (btrim(subscription_id) <> ''),
    provider_item_id                TEXT NOT NULL CHECK (btrim(provider_item_id) <> ''),
    allocation_reference            TEXT NOT NULL CHECK (btrim(allocation_reference) <> ''),
    beneficiary_id                  TEXT NOT NULL CHECK (btrim(beneficiary_id) <> ''),
    allocation_id                   TEXT NOT NULL CHECK (btrim(allocation_id) <> ''),
    coverage_source_id              TEXT NOT NULL CHECK (btrim(coverage_source_id) <> ''),
    predecessor_period_start        BIGINT NOT NULL CHECK (predecessor_period_start >= 0),
    predecessor_period_end          BIGINT NOT NULL
                                     CHECK (predecessor_period_end > predecessor_period_start),
    renewal_period_start            BIGINT NOT NULL CHECK (renewal_period_start >= 0),
    renewal_period_end              BIGINT NOT NULL
                                     CHECK (renewal_period_end > renewal_period_start),
    event_created_at                BIGINT NOT NULL CHECK (event_created_at >= 0),
    interval                        TEXT NOT NULL CHECK (interval IN ('month', 'year')),
    accepted_generation             BIGINT NOT NULL CHECK (accepted_generation > 0),
    created_at                      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_namespace, provider_account_id, provider_environment, event_id),
    UNIQUE (provider_namespace, provider_account_id, provider_environment, evidence_reference),
    CONSTRAINT cloud_provider_stripe_renewal_failure_association_fk
        FOREIGN KEY (provider_namespace, provider_account_id, provider_environment, event_id)
        REFERENCES cloud_provider_invalidation_associations
            (provider_namespace, provider_account_id, provider_environment, event_id)
        ON DELETE RESTRICT,
    CONSTRAINT cloud_provider_stripe_renewal_failure_allocation_fk
        FOREIGN KEY (allocation_id) REFERENCES cloud_provider_allocations (allocation_id)
        ON DELETE RESTRICT,
    CONSTRAINT cloud_provider_stripe_renewal_failure_source_fk
        FOREIGN KEY (coverage_source_id) REFERENCES cloud_coverage_sources (source_id)
        ON DELETE RESTRICT,
    CONSTRAINT cloud_provider_stripe_renewal_failure_period_adjacency
        CHECK (predecessor_period_end = renewal_period_start)
);

CREATE INDEX cloud_provider_stripe_renewal_failures_allocation_idx
    ON cloud_provider_stripe_renewal_failures
        (beneficiary_id, allocation_id, event_created_at);
