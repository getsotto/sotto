-- Keep both provider subscriptions on a deletion operation when an organisation still has a
-- legacy Team subscription alongside its sponsored seat subscription. Purge must observe and
-- cancel every provider billing resource before releasing the organisation tombstone.
ALTER TABLE organization_deletions
    ADD COLUMN sponsored_subscription_id TEXT;

ALTER TABLE organization_deletions
    ADD CONSTRAINT organization_deletions_sponsored_subscription_id_check
    CHECK (
        sponsored_subscription_id IS NULL
        OR btrim(sponsored_subscription_id) <> ''
    );
