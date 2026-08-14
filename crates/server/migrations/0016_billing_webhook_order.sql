-- Stripe webhook delivery is at-least-once and can arrive out of order. Keep event ids for
-- redelivery deduplication and a per-subscription watermark for ordering distinct events.

CREATE TABLE stripe_webhook_events (
    event_id TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    stripe_created BIGINT NOT NULL CHECK (stripe_created >= 0),
    subscription_id TEXT,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE stripe_subscription_watermarks (
    subscription_id TEXT PRIMARY KEY,
    stripe_created BIGINT NOT NULL CHECK (stripe_created >= 0),
    event_id TEXT NOT NULL
        CONSTRAINT stripe_subscription_watermarks_event_fk
        REFERENCES stripe_webhook_events (event_id) ON DELETE RESTRICT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Webhook handling locks one subscription's watermark, never the whole event history.
CREATE INDEX stripe_webhook_events_subscription_idx
    ON stripe_webhook_events (subscription_id, stripe_created);
