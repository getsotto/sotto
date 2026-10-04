-- The allocation row is the durable ownership interval.  This table records the Stripe price
-- that bought that interval without copying provider identity fields into a second mutable row.
-- A new allocation (and therefore a new source id) is required when a named seat changes price or
-- ends; that keeps every manifest interval independently attributable and makes mid-term removal
-- explicit through effective_until.
ALTER TABLE cloud_provider_allocations
    ADD COLUMN effective_until_evidence_reference TEXT
        CHECK (effective_until_evidence_reference IS NULL
            OR btrim(effective_until_evidence_reference) <> '');

CREATE TABLE cloud_provider_sponsored_allocation_terms (
    allocation_id TEXT PRIMARY KEY
        REFERENCES cloud_provider_allocations (allocation_id) ON DELETE RESTRICT,
    price_id TEXT NOT NULL CHECK (btrim(price_id) <> ''),
    effective_from BIGINT NOT NULL CHECK (effective_from >= 0),
    effective_until BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (effective_until IS NULL OR effective_from < effective_until)
);

CREATE INDEX cloud_provider_sponsored_allocation_terms_lookup_idx
    ON cloud_provider_sponsored_allocation_terms (effective_from, effective_until);
