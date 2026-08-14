-- Stripe webhook delivery is at-least-once and can arrive out of order. Keep event ids for
-- redelivery deduplication and a per-subscription watermark for ordering distinct events.

CREATE TABLE stripe_webhook_events (
    -- The provider id is the redelivery key; keeping it immutable makes retries harmless.
    event_id TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    -- Stripe's creation time is the ordering source, not the time this server received the event.
    stripe_created BIGINT NOT NULL CHECK (stripe_created >= 0),
    subscription_id TEXT,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Equal-time reconciliation may call Stripe after this row is committed; leave it pending
    -- until the provider result and watermark have been applied successfully.
    processed_at TIMESTAMPTZ
);

CREATE TABLE stripe_subscription_watermarks (
    -- One row serialises distinct events for each subscription across server instances.
    subscription_id TEXT PRIMARY KEY,
    stripe_created BIGINT NOT NULL CHECK (stripe_created >= 0),
    -- Retain the event that established the watermark so the ordering decision is auditable.
    event_id TEXT NOT NULL
        CONSTRAINT stripe_subscription_watermarks_event_fk
        REFERENCES stripe_webhook_events (event_id) ON DELETE RESTRICT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
