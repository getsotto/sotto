-- A sponsored Stripe subscription item can cover several named beneficiaries at once.
-- Allocation references and coverage sources remain unique; the provider item itself is not.

-- Keep payer kind on the allocation row so PostgreSQL can enforce the personal-item invariant
-- with a partial unique index. The default preserves direct legacy fixture inserts; existing
-- sponsor rows are backfilled from their payer before the index is created.
ALTER TABLE cloud_provider_allocations
    ADD COLUMN payer_kind TEXT NOT NULL DEFAULT 'personal'
        CHECK (payer_kind IN ('personal', 'sponsor'));

UPDATE cloud_provider_allocations AS allocation
SET payer_kind = payer.payer_kind
FROM cloud_provider_payers AS payer
WHERE payer.payer_id = allocation.payer_id;

-- PostgreSQL truncates generated identifiers to 63 bytes (and may suffix collisions), so resolve
-- the old constraint by its exact ordered column set rather than guessing its generated name.
DO $$
DECLARE
    old_constraint TEXT;
BEGIN
    SELECT constraint_name
    INTO old_constraint
    FROM (
        SELECT
            con.conname AS constraint_name,
            ARRAY(
                SELECT attribute.attname
                FROM unnest(con.conkey) WITH ORDINALITY AS column_key(attnum, position)
                JOIN pg_attribute AS attribute
                  ON attribute.attrelid = con.conrelid
                 AND attribute.attnum = column_key.attnum
                ORDER BY column_key.position
            ) AS columns
        FROM pg_constraint AS con
        WHERE con.conrelid = 'cloud_provider_allocations'::regclass
          AND con.contype = 'u'
    ) AS unique_constraints
    WHERE columns = ARRAY[
        'provider_namespace',
        'provider_account_id',
        'provider_environment',
        'provider_subscription_id',
        'provider_item_id'
    ]::name[];

    IF old_constraint IS NULL THEN
        RAISE EXCEPTION
            'cloud_provider_allocations provider-item uniqueness constraint was not found';
    END IF;

    EXECUTE format(
        'ALTER TABLE cloud_provider_allocations DROP CONSTRAINT %I',
        old_constraint
    );
END
$$;

-- Personal allocations still represent one quantity-one Stripe item. Only sponsor rows may
-- share an item, because the sponsored manifest is the boundary that attributes its quantity.
CREATE UNIQUE INDEX cloud_provider_allocations_personal_provider_item_key
    ON cloud_provider_allocations (
        provider_namespace,
        provider_account_id,
        provider_environment,
        provider_subscription_id,
        provider_item_id
    )
    WHERE payer_kind = 'personal';

CREATE INDEX cloud_provider_allocations_provider_item_idx
    ON cloud_provider_allocations (
        provider_namespace,
        provider_account_id,
        provider_environment,
        provider_subscription_id,
        provider_item_id,
        effective_from
    );
