-- Fence provider collections when a verified provider change is accepted.
-- Existing completed history remains readable; old pending attempts are deliberately unfenced.

ALTER TABLE cloud_coverage_coordinators
    ADD COLUMN provider_invalidation_generation BIGINT NOT NULL DEFAULT 0
        CHECK (provider_invalidation_generation >= 0);

ALTER TABLE cloud_coverage_collection_attempts
    ADD COLUMN provider_invalidation_generation BIGINT
        CHECK (provider_invalidation_generation IS NULL OR provider_invalidation_generation >= 0);

CREATE TABLE cloud_provider_invalidation_associations (
    provider_namespace       TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id      TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment     TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id                 TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    event_type               TEXT NOT NULL CHECK (btrim(event_type) <> ''),
    provider_created_at      BIGINT NOT NULL CHECK (provider_created_at >= 0),
    normalized_payload_hash  TEXT NOT NULL CHECK (normalized_payload_hash ~ '^[0-9a-f]{64}$'),
    beneficiary_id           TEXT NOT NULL REFERENCES cloud_coverage_coordinators (beneficiary_id)
                                  ON DELETE RESTRICT,
    allocation_id            TEXT NOT NULL REFERENCES cloud_provider_allocations (allocation_id)
                                  ON DELETE RESTRICT,
    coverage_source_id       TEXT NOT NULL REFERENCES cloud_coverage_sources (source_id)
                                  ON DELETE RESTRICT,
    accepted_generation      BIGINT NOT NULL CHECK (accepted_generation > 0),
    accepted_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_namespace, provider_account_id, provider_environment, event_id),
    CONSTRAINT cloud_provider_invalidation_event_fk
        FOREIGN KEY (provider_namespace, provider_account_id, provider_environment, event_id)
        REFERENCES cloud_provider_event_receipts
            (provider_namespace, provider_account_id, provider_environment, event_id)
        ON DELETE RESTRICT,
    UNIQUE (beneficiary_id, accepted_generation)
);

CREATE INDEX cloud_provider_invalidation_beneficiary_idx
    ON cloud_provider_invalidation_associations (beneficiary_id, accepted_generation);
