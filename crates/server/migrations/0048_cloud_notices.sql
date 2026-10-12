-- Durable lifecycle notices are separate from encrypted product data.  The payload contains only
-- bounded, user-facing dates/amounts and is never allowed to carry secret names or ciphertext.

CREATE TABLE IF NOT EXISTS cloud_verified_contacts (
    contact_id       TEXT PRIMARY KEY,
    user_id          TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    channel          TEXT NOT NULL,
    destination      TEXT NOT NULL,
    verified_at      TIMESTAMPTZ NOT NULL,
    revoked_at       TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (channel IN ('email')),
    CHECK (length(destination) BETWEEN 3 AND 320)
);

CREATE UNIQUE INDEX IF NOT EXISTS cloud_verified_contacts_active_idx
    ON cloud_verified_contacts (user_id, channel)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS cloud_notice_outbox (
    notice_id            TEXT PRIMARY KEY,
    recipient_user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    event_key            TEXT NOT NULL,
    policy_key           TEXT NOT NULL,
    kind                 TEXT NOT NULL,
    channel              TEXT NOT NULL,
    contact_id           TEXT REFERENCES cloud_verified_contacts (contact_id) ON DELETE SET NULL,
    payload              JSONB NOT NULL,
    due_at               TIMESTAMPTZ NOT NULL,
    available_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    status               TEXT NOT NULL DEFAULT 'pending',
    attempt_count        INTEGER NOT NULL DEFAULT 0,
    lease_owner          TEXT,
    lease_expires_at     TIMESTAMPTZ,
    last_error_code      TEXT,
    delivered_at         TIMESTAMPTZ,
    cancelled_at         TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (channel IN ('in_app', 'email')),
    CHECK (status IN ('pending', 'leased', 'delivered', 'cancelled', 'failed')),
    CHECK (attempt_count >= 0),
    -- A deleted contact is nulled by the foreign key. The worker revalidates before sending and
    -- records contact_missing, while enqueue still requires an active verified contact.
    CHECK ((channel = 'in_app' AND contact_id IS NULL) OR channel = 'email')
);

CREATE UNIQUE INDEX IF NOT EXISTS cloud_notice_outbox_identity_idx
    ON cloud_notice_outbox (recipient_user_id, event_key, policy_key, channel);

CREATE INDEX IF NOT EXISTS cloud_notice_outbox_due_idx
    ON cloud_notice_outbox (status, available_at, lease_expires_at);

CREATE INDEX IF NOT EXISTS cloud_notice_outbox_recipient_idx
    ON cloud_notice_outbox (recipient_user_id, created_at DESC);
