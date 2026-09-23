-- Durable, provider-neutral evidence for the first Cloud coverage adapter.
-- Raw provider payloads and credentials never belong in these tables.

CREATE TABLE cloud_provider_payers (
    payer_id                TEXT PRIMARY KEY CHECK (btrim(payer_id) <> ''),
    provider_namespace      TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id     TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment    TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    provider_customer_id    TEXT NOT NULL CHECK (btrim(provider_customer_id) <> ''),
    payer_kind              TEXT NOT NULL CHECK (payer_kind IN ('personal', 'sponsor')),
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider_namespace, provider_account_id, provider_environment, provider_customer_id)
);

CREATE TABLE cloud_provider_allocations (
    allocation_id                 TEXT PRIMARY KEY CHECK (btrim(allocation_id) <> ''),
    payer_id                      TEXT NOT NULL REFERENCES cloud_provider_payers (payer_id)
                                      ON DELETE RESTRICT,
    beneficiary_id                TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    provider_namespace            TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id           TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment          TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    provider_subscription_id      TEXT NOT NULL CHECK (btrim(provider_subscription_id) <> ''),
    provider_item_id              TEXT NOT NULL CHECK (btrim(provider_item_id) <> ''),
    external_allocation_reference TEXT NOT NULL
                                      CHECK (btrim(external_allocation_reference) <> ''),
    coverage_source_id             TEXT NOT NULL UNIQUE
                                      REFERENCES cloud_coverage_sources (source_id)
                                      DEFERRABLE INITIALLY DEFERRED,
    effective_from                BIGINT NOT NULL,
    effective_until               BIGINT,
    state                         TEXT NOT NULL CHECK (state IN ('pending', 'active', 'ended')),
    ownership_evidence_reference  TEXT NOT NULL
                                      CHECK (btrim(ownership_evidence_reference) <> ''),
    created_at                    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (effective_until IS NULL OR effective_from < effective_until),
    UNIQUE (
        provider_namespace,
        provider_account_id,
        provider_environment,
        external_allocation_reference
    ),
    UNIQUE (
        provider_namespace,
        provider_account_id,
        provider_environment,
        provider_subscription_id,
        provider_item_id
    )
);

CREATE INDEX cloud_provider_allocations_beneficiary_idx
    ON cloud_provider_allocations (beneficiary_id, state);

CREATE TABLE cloud_provider_event_receipts (
    provider_namespace       TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id      TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment     TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id                 TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    event_type               TEXT NOT NULL CHECK (btrim(event_type) <> ''),
    provider_created_at      BIGINT NOT NULL CHECK (provider_created_at >= 0),
    subscription_id          TEXT CHECK (subscription_id IS NULL OR btrim(subscription_id) <> ''),
    allocation_reference     TEXT CHECK (
        allocation_reference IS NULL OR btrim(allocation_reference) <> ''
    ),
    normalized_payload_hash  TEXT NOT NULL CHECK (normalized_payload_hash ~ '^[0-9a-f]{64}$'),
    status                   TEXT NOT NULL CHECK (status IN ('pending', 'applied', 'rejected')),
    rejection_code           TEXT,
    allocation_id            TEXT,
    coverage_source_id       TEXT,
    projection_revision      BIGINT CHECK (projection_revision IS NULL OR projection_revision > 0),
    received_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at             TIMESTAMPTZ,
    PRIMARY KEY (provider_namespace, provider_account_id, provider_environment, event_id),
    CHECK (
        (status = 'applied' AND processed_at IS NOT NULL AND rejection_code IS NULL
            AND allocation_id IS NOT NULL AND coverage_source_id IS NOT NULL
            AND projection_revision IS NOT NULL)
        OR (status = 'rejected' AND processed_at IS NOT NULL AND rejection_code IS NOT NULL
            AND allocation_id IS NULL AND coverage_source_id IS NULL
            AND projection_revision IS NULL)
        OR (status = 'pending' AND processed_at IS NULL AND rejection_code IS NULL
            AND allocation_id IS NULL AND coverage_source_id IS NULL
            AND projection_revision IS NULL)
    )
);

CREATE INDEX cloud_provider_event_receipts_pending_idx
    ON cloud_provider_event_receipts (received_at)
    WHERE status = 'pending';
