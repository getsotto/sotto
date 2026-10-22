-- Email addresses are usable for lifecycle notices only after a one-use challenge is confirmed.
-- Raw challenge tokens are never persisted; only a SHA-256 digest is stored.
CREATE TABLE cloud_contact_verification_challenges (
    challenge_id       TEXT PRIMARY KEY CHECK (btrim(challenge_id) <> ''),
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    destination        TEXT NOT NULL CHECK (length(destination) BETWEEN 3 AND 320),
    destination_digest TEXT NOT NULL CHECK (destination_digest ~ '^[0-9a-f]{64}$'),
    token_digest       BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    idempotency_key    TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    request_digest     TEXT NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    status             TEXT NOT NULL CHECK (status IN (
                           'sending', 'pending', 'delivery_unknown', 'verified',
                           'superseded', 'expired', 'failed'
                       )),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at         TIMESTAMPTZ NOT NULL,
    verified_at        TIMESTAMPTZ,
    last_error_code    TEXT,
    UNIQUE (user_id, idempotency_key),
    CHECK (expires_at > created_at),
    CHECK ((status = 'verified') = (verified_at IS NOT NULL))
);

CREATE UNIQUE INDEX cloud_contact_one_live_challenge_per_user_idx
    ON cloud_contact_verification_challenges (user_id)
    WHERE status IN ('sending', 'pending', 'delivery_unknown');

CREATE INDEX cloud_contact_challenges_expiry_idx
    ON cloud_contact_verification_challenges (expires_at)
    WHERE status IN ('sending', 'pending', 'delivery_unknown');

-- The message identity is durable before contacting the provider. A retry must reuse this
-- identity according to the configured provider's idempotency contract.
CREATE TABLE cloud_contact_verification_send_attempts (
    challenge_id       TEXT NOT NULL
                       REFERENCES cloud_contact_verification_challenges (challenge_id)
                       ON DELETE CASCADE,
    attempt_number     INTEGER NOT NULL CHECK (attempt_number > 0),
    message_id         TEXT NOT NULL UNIQUE CHECK (btrim(message_id) <> ''),
    outcome            TEXT NOT NULL CHECK (outcome IN ('prepared', 'accepted', 'unknown', 'rejected')),
    provider_message_id TEXT,
    error_code         TEXT,
    started_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at       TIMESTAMPTZ,
    PRIMARY KEY (challenge_id, attempt_number),
    CHECK ((outcome = 'prepared') = (completed_at IS NULL)),
    CHECK ((outcome = 'accepted') = (provider_message_id IS NOT NULL))
);

-- Enforce both account-wide and destination-wide send budgets without retaining client IPs.
-- Per-IP throttling is applied at the bundled edge proxy before requests reach the API.
CREATE TABLE cloud_contact_verification_rate_limits (
    scope_kind          TEXT NOT NULL CHECK (scope_kind IN ('account', 'destination')),
    scope_digest        TEXT NOT NULL CHECK (scope_digest ~ '^[0-9a-f]{64}$'),
    window_started_at   TIMESTAMPTZ NOT NULL,
    attempt_count       INTEGER NOT NULL CHECK (attempt_count >= 0),
    PRIMARY KEY (scope_kind, scope_digest)
);

CREATE INDEX cloud_contact_verification_rate_limits_window_idx
    ON cloud_contact_verification_rate_limits (window_started_at);

CREATE INDEX cloud_contact_verification_challenge_cleanup_idx
    ON cloud_contact_verification_challenges (created_at);

-- Older internal enrolment allowed the same address on multiple accounts. There is no safe
-- account to choose as its owner, so revoke every ambiguous legacy claim before enforcing the
-- new one-account-per-address rule. Users can verify the address again on the correct account.
WITH ambiguous_destinations AS (
    SELECT lower(destination) AS normalized_destination
    FROM cloud_verified_contacts
    WHERE revoked_at IS NULL
    GROUP BY lower(destination)
    HAVING count(*) > 1
)
UPDATE cloud_verified_contacts AS contact
SET revoked_at = now()
FROM ambiguous_destinations AS ambiguous
WHERE contact.revoked_at IS NULL
  AND lower(contact.destination) = ambiguous.normalized_destination;

CREATE UNIQUE INDEX cloud_verified_contacts_destination_active_idx
    ON cloud_verified_contacts (lower(destination))
    WHERE revoked_at IS NULL;
