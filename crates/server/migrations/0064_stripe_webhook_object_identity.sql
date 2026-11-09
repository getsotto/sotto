-- Keep the provider object identity with the durable receipt. Billing decisions that race across
-- different invoice events need to distinguish an already-processed payment for this invoice.
ALTER TABLE stripe_webhook_events
    ADD COLUMN provider_object_id TEXT;

ALTER TABLE stripe_webhook_events
    ADD CONSTRAINT stripe_webhook_events_provider_object_id_nonempty
    CHECK (provider_object_id IS NULL OR btrim(provider_object_id) <> '');

CREATE INDEX stripe_webhook_events_processed_invoice_idx
    ON stripe_webhook_events (subscription_id, provider_object_id)
    WHERE event_type = 'invoice.paid' AND processed_at IS NOT NULL;
