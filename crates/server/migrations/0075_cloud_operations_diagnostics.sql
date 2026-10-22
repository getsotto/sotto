-- Preserve paid invoices whose Cloud coverage evidence was permanently rejected. This record is
-- diagnostic and repairable (or explicitly unrecoverable); it never changes billing, coverage,
-- eligibility, or retention state.
CREATE TABLE cloud_coverage_rejection_incidents (
    fingerprint          TEXT PRIMARY KEY CHECK (fingerprint ~ '^[0-9a-f]{64}$'),
    provider_account_id  TEXT NOT NULL CHECK (btrim(provider_account_id) <> ''),
    provider_environment TEXT NOT NULL CHECK (provider_environment IN ('test', 'live')),
    event_id              TEXT NOT NULL CHECK (btrim(event_id) <> ''),
    event_type            TEXT NOT NULL CHECK (event_type = 'invoice.paid'),
    coverage_kind         TEXT NOT NULL CHECK (coverage_kind IN ('personal', 'sponsored')),
    reason_code           TEXT NOT NULL CHECK (reason_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    state                 TEXT NOT NULL DEFAULT 'open'
                          CHECK (state IN ('open', 'repairing', 'resolved', 'unrecoverable')),
    repair_attempt_count  INTEGER NOT NULL DEFAULT 0 CHECK (repair_attempt_count >= 0),
    repair_lease_token    TEXT,
    repair_lease_until    TIMESTAMPTZ,
    last_attempt_at       TIMESTAMPTZ,
    resolved_at           TIMESTAMPTZ,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider_account_id, provider_environment, event_id, coverage_kind),
    CHECK ((repair_lease_token IS NULL) = (repair_lease_until IS NULL)),
    CHECK ((state = 'repairing') = (repair_lease_token IS NOT NULL)),
    CHECK ((state = 'resolved') = (resolved_at IS NOT NULL))
);

CREATE INDEX cloud_coverage_rejection_incidents_open_idx
    ON cloud_coverage_rejection_incidents
        (provider_account_id, provider_environment, created_at, fingerprint)
    WHERE state IN ('open', 'repairing');

-- Actor labels are declared attribution only: the route uses one shared, dedicated repair
-- credential, so the label is not proof of the caller's personal identity.
CREATE TABLE cloud_coverage_rejection_actions (
    action_id     BIGSERIAL PRIMARY KEY,
    fingerprint   TEXT NOT NULL REFERENCES cloud_coverage_rejection_incidents (fingerprint)
                  ON DELETE RESTRICT,
    actor_label   TEXT NOT NULL CHECK (
        length(btrim(actor_label)) BETWEEN 1 AND 80 AND actor_label !~ '[[:cntrl:]]'
    ),
    outcome       TEXT NOT NULL CHECK (outcome IN (
        'applied', 'already_applied', 'still_rejected', 'unresolved', 'provider_error',
        'event_unavailable'
    )),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX cloud_coverage_rejection_actions_history_idx
    ON cloud_coverage_rejection_actions (fingerprint, action_id DESC);
