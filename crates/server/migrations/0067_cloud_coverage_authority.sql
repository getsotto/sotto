-- A projection is usable for Cloud authority only when it was produced by a complete,
-- generation-fenced collection and remains within its explicit freshness window.
CREATE TABLE cloud_coverage_authority_receipts (
    beneficiary_id                    TEXT NOT NULL,
    projection_revision               BIGINT NOT NULL CHECK (projection_revision > 0),
    attempt_id                        TEXT NOT NULL CHECK (btrim(attempt_id) <> ''),
    provider_namespace                TEXT NOT NULL CHECK (btrim(provider_namespace) <> ''),
    provider_account_id               TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment              TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    source_set_generation              BIGINT NOT NULL CHECK (source_set_generation > 0),
    provider_invalidation_generation  BIGINT NOT NULL CHECK (provider_invalidation_generation >= 0),
    policy_version                    INTEGER NOT NULL CHECK (policy_version > 0),
    local_correction_digest            TEXT NOT NULL CHECK (local_correction_digest ~ '^[0-9a-f]{64}$'),
    observed_at                       TIMESTAMPTZ NOT NULL,
    fresh_until                       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (beneficiary_id, projection_revision),
    UNIQUE (attempt_id),
    FOREIGN KEY (beneficiary_id, projection_revision)
        REFERENCES cloud_coverage_revisions (beneficiary_id, revision) ON DELETE RESTRICT,
    FOREIGN KEY (beneficiary_id, attempt_id)
        REFERENCES cloud_coverage_collection_attempts (beneficiary_id, attempt_id) ON DELETE RESTRICT,
    CHECK (observed_at < fresh_until)
);

CREATE INDEX cloud_coverage_authority_freshness_idx
    ON cloud_coverage_authority_receipts (fresh_until, beneficiary_id);

-- Periodic revalidation has its own identity and lifecycle. It is not a fabricated Stripe event
-- and is deliberately separate from the event-repair queue.
CREATE TABLE cloud_coverage_revalidation_jobs (
    beneficiary_id       TEXT PRIMARY KEY
        REFERENCES cloud_coverage_coordinators (beneficiary_id) ON DELETE CASCADE,
    status               TEXT NOT NULL DEFAULT 'pending'
                         CHECK (status IN ('pending', 'leased')),
    next_attempt_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempt_count        INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    lease_owner          TEXT,
    lease_token          TEXT,
    lease_expires_at     TIMESTAMPTZ,
    last_error_code      TEXT,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((status = 'leased') =
           (lease_owner IS NOT NULL AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL))
);

INSERT INTO cloud_coverage_revalidation_jobs (beneficiary_id)
SELECT coordinator.beneficiary_id
FROM cloud_coverage_coordinators AS coordinator
WHERE EXISTS (
    SELECT 1 FROM cloud_coverage_sources AS source
    WHERE source.beneficiary_id = coordinator.beneficiary_id
      AND NOT EXISTS (
          SELECT 1 FROM cloud_coverage_source_retirements AS retirement
          WHERE retirement.beneficiary_id = source.beneficiary_id
            AND retirement.source_id = source.source_id
      )
);

CREATE INDEX cloud_coverage_revalidation_due_idx
    ON cloud_coverage_revalidation_jobs (next_attempt_at, beneficiary_id)
    WHERE status = 'pending';

CREATE INDEX cloud_coverage_revalidation_lease_idx
    ON cloud_coverage_revalidation_jobs (lease_expires_at, beneficiary_id)
    WHERE status = 'leased';

CREATE FUNCTION enqueue_cloud_coverage_revalidation() RETURNS trigger AS $$
BEGIN
    INSERT INTO cloud_coverage_revalidation_jobs (beneficiary_id)
    SELECT coordinator.beneficiary_id FROM cloud_coverage_coordinators AS coordinator
    WHERE coordinator.beneficiary_id = NEW.beneficiary_id
    ON CONFLICT (beneficiary_id) DO UPDATE
    SET status = 'pending', next_attempt_at = now(), attempt_count = 0,
        lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
        last_error_code = NULL, updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER cloud_coverage_sources_enqueue_revalidation
AFTER INSERT ON cloud_coverage_sources
FOR EACH ROW EXECUTE FUNCTION enqueue_cloud_coverage_revalidation();

CREATE TRIGGER cloud_coverage_retirements_enqueue_revalidation
AFTER INSERT ON cloud_coverage_source_retirements
FOR EACH ROW EXECUTE FUNCTION enqueue_cloud_coverage_revalidation();

CREATE FUNCTION invalidate_authority_after_allocation_change() RETURNS trigger AS $$
BEGIN
    IF ROW(NEW.effective_until, NEW.state) IS DISTINCT FROM ROW(OLD.effective_until, OLD.state) THEN
        UPDATE cloud_coverage_coordinators
        SET provider_invalidation_generation = provider_invalidation_generation + 1
        WHERE beneficiary_id = NEW.beneficiary_id;
        INSERT INTO cloud_coverage_revalidation_jobs (beneficiary_id)
        SELECT beneficiary_id FROM cloud_coverage_coordinators
        WHERE beneficiary_id = NEW.beneficiary_id
        ON CONFLICT (beneficiary_id) DO UPDATE
        SET status = 'pending', next_attempt_at = now(), attempt_count = 0,
            lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
            last_error_code = NULL, updated_at = now();
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER cloud_provider_allocations_invalidate_authority
AFTER UPDATE OF effective_until, state ON cloud_provider_allocations
FOR EACH ROW EXECUTE FUNCTION invalidate_authority_after_allocation_change();

CREATE FUNCTION reject_cloud_coverage_authority_receipt_update() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'cloud coverage authority receipts cannot be updated';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER cloud_coverage_authority_receipts_immutable
BEFORE UPDATE ON cloud_coverage_authority_receipts
FOR EACH ROW EXECUTE FUNCTION reject_cloud_coverage_authority_receipt_update();

CREATE FUNCTION invalidate_authority_for_confirmed_personal_termination() RETURNS trigger AS $$
DECLARE
    changed BOOLEAN := TRUE;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        changed := ROW(NEW.state, NEW.full_refund_requested, NEW.preserve_paid_term,
                       NEW.early_termination_confirmed_at_epoch, NEW.effective_at_epoch)
                   IS DISTINCT FROM
                   ROW(OLD.state, OLD.full_refund_requested, OLD.preserve_paid_term,
                       OLD.early_termination_confirmed_at_epoch, OLD.effective_at_epoch);
    END IF;
    IF changed AND NEW.payer_kind = 'personal' AND NEW.state = 'refunded'
       AND NEW.full_refund_requested AND NOT NEW.preserve_paid_term
       AND NEW.early_termination_confirmed_at_epoch IS NOT NULL
       AND NEW.effective_at_epoch IS NOT NULL THEN
        -- This generation is also the authority invalidation fence for verified local correction
        -- evidence; provider events and explicit early termination both invalidate old reads.
        UPDATE cloud_coverage_coordinators
        SET provider_invalidation_generation = provider_invalidation_generation + 1
        WHERE beneficiary_id = NEW.beneficiary_id;
        INSERT INTO cloud_coverage_revalidation_jobs (beneficiary_id)
        SELECT beneficiary_id FROM cloud_coverage_coordinators
        WHERE beneficiary_id = NEW.beneficiary_id
        ON CONFLICT (beneficiary_id) DO UPDATE
        SET status = 'pending', next_attempt_at = now(), attempt_count = 0,
            lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
            last_error_code = NULL, updated_at = now();
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER billing_correction_invalidates_coverage_authority
AFTER INSERT OR UPDATE OF state, full_refund_requested, preserve_paid_term,
    early_termination_confirmed_at_epoch, effective_at_epoch
ON billing_correction_requests
FOR EACH ROW EXECUTE FUNCTION invalidate_authority_for_confirmed_personal_termination();
