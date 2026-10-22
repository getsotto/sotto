-- Relay acceptance, confirmed delivery and suppression are separate outcomes. This migration
-- keeps the customer-visible lifecycle intent immutable while recording bounded provider state.
ALTER TABLE cloud_notice_outbox
    DROP CONSTRAINT cloud_notice_outbox_status_check,
    ADD COLUMN provider_message_id TEXT,
    ADD COLUMN scope_id TEXT CHECK (scope_id IS NULL OR btrim(scope_id) <> ''),
    ADD COLUMN provider_first_attempt_at TIMESTAMPTZ,
    ADD COLUMN provider_accepted_at TIMESTAMPTZ,
    ADD COLUMN provider_delivered_at TIMESTAMPTZ,
    ADD CONSTRAINT cloud_notice_outbox_status_check CHECK (status IN (
        'pending', 'leased', 'accepted', 'delivery_unknown', 'delivered',
        'cancelled', 'failed', 'bounced', 'suppressed'
    ));

ALTER TABLE cloud_notice_outbox ADD COLUMN operator_fingerprint TEXT
    GENERATED ALWAYS AS (md5(notice_id)) STORED;
ALTER TABLE cloud_notice_outbox ADD COLUMN operator_retry_count INTEGER NOT NULL DEFAULT 0
    CHECK (operator_retry_count >= 0 AND operator_retry_count <= 2);
CREATE UNIQUE INDEX cloud_notice_outbox_operator_fingerprint_idx
    ON cloud_notice_outbox (operator_fingerprint);

CREATE UNIQUE INDEX cloud_notice_outbox_provider_message_idx
    ON cloud_notice_outbox (provider_message_id)
    WHERE provider_message_id IS NOT NULL;

-- Do not retain provider payloads or addresses. Signed callbacks are deduplicated by provider
-- event id and reduced to the fields needed to update one outbox row.
CREATE TABLE cloud_notice_provider_events (
    provider_event_id       TEXT PRIMARY KEY CHECK (btrim(provider_event_id) <> ''),
    provider_message_id     TEXT NOT NULL CHECK (btrim(provider_message_id) <> ''),
    outcome                 TEXT NOT NULL CHECK (outcome IN (
                                'sent', 'delivered', 'bounced', 'failed', 'complained'
                            )),
    occurred_at             TIMESTAMPTZ,
    received_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX cloud_notice_provider_events_message_idx
    ON cloud_notice_provider_events (provider_message_id, received_at);

CREATE TABLE cloud_notice_operator_actions (
    action_id          BIGSERIAL PRIMARY KEY,
    notice_fingerprint TEXT NOT NULL CHECK (notice_fingerprint ~ '^[0-9a-f]{32}$'),
    operator_label     TEXT NOT NULL CHECK (length(btrim(operator_label)) BETWEEN 1 AND 80),
    action             TEXT NOT NULL CHECK (action IN ('retry', 'suppress')),
    applied            BOOLEAN NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX cloud_notice_operator_actions_notice_idx
    ON cloud_notice_operator_actions (notice_fingerprint, created_at DESC);
