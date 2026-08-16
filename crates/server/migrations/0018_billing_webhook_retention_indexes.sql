-- Keep receipt cleanup bounded without scanning pending rows or the referenced watermark table.

CREATE INDEX stripe_subscription_watermarks_event_idx
    ON stripe_subscription_watermarks (event_id);

CREATE INDEX stripe_webhook_events_pending_idx
    ON stripe_webhook_events (received_at)
    WHERE processed_at IS NULL;
