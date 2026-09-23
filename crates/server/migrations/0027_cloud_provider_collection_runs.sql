-- Associate a pending provider receipt with the exact beneficiary collection attempt that it
-- prepared. The composite reference prevents a completion from crossing beneficiary boundaries.

ALTER TABLE cloud_provider_event_receipts
    ADD COLUMN collection_beneficiary_id TEXT,
    ADD COLUMN collection_attempt_id TEXT,
    ADD COLUMN collection_run_id TEXT;

ALTER TABLE cloud_provider_event_receipts
    ADD CONSTRAINT cloud_provider_receipts_collection_identity_check
    CHECK (
        (collection_beneficiary_id IS NULL AND collection_attempt_id IS NULL
            AND collection_run_id IS NULL)
        OR (collection_beneficiary_id IS NOT NULL
            AND collection_attempt_id IS NOT NULL
            AND collection_run_id IS NOT NULL
            AND btrim(collection_beneficiary_id) <> ''
            AND btrim(collection_attempt_id) <> ''
            AND btrim(collection_run_id) <> '')
    );

ALTER TABLE cloud_provider_event_receipts
    ADD CONSTRAINT cloud_provider_receipts_collection_attempt_fk
    FOREIGN KEY (collection_beneficiary_id, collection_attempt_id)
    REFERENCES cloud_coverage_collection_attempts (beneficiary_id, attempt_id)
    DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX cloud_provider_event_receipts_collection_attempt_idx
    ON cloud_provider_event_receipts (collection_beneficiary_id, collection_attempt_id)
    WHERE collection_attempt_id IS NOT NULL;
