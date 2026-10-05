-- Keep the provider object's stable identity with the verified event receipt so a restart-safe
-- refresh worker can reread the same object without retaining the webhook payload.
ALTER TABLE cloud_provider_event_receipts
    ADD COLUMN provider_object_id TEXT
        CHECK (provider_object_id IS NULL OR btrim(provider_object_id) <> '');
