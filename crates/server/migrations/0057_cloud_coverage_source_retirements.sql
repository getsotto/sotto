-- Preserve source ownership history while allowing a provider-verified payer transfer to retire
-- the old allocation from future complete-source snapshots.
CREATE TABLE cloud_coverage_source_retirements (
    source_id TEXT PRIMARY KEY REFERENCES cloud_coverage_sources (source_id) ON DELETE RESTRICT,
    beneficiary_id TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    effective_until_epoch BIGINT NOT NULL CHECK (effective_until_epoch > 0),
    retirement_operation_id TEXT NOT NULL CHECK (btrim(retirement_operation_id) <> ''),
    provider_end_reference TEXT NOT NULL CHECK (btrim(provider_end_reference) <> ''),
    source_set_generation BIGINT NOT NULL CHECK (source_set_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (beneficiary_id, retirement_operation_id)
);

CREATE INDEX cloud_coverage_source_retirements_beneficiary_idx
    ON cloud_coverage_source_retirements (beneficiary_id, source_set_generation);
