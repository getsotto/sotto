-- Keep the Stripe API version that arrived with each receipt so mismatches are auditable.

ALTER TABLE stripe_webhook_events
    ADD COLUMN api_version TEXT NOT NULL DEFAULT 'unknown';

ALTER TABLE stripe_webhook_events
    ALTER COLUMN api_version DROP DEFAULT;
