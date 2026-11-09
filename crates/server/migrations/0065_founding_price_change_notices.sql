-- Existing awards are deliberately ineligible for automatic notice backfill. New award creation
-- and an explicit payer transfer opt an award in through the founding-schedule enqueue path.
ALTER TABLE billing_founding_schedule_jobs
    ADD COLUMN price_notice_eligible BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX billing_founding_price_notice_candidates_idx
    ON billing_founding_schedule_jobs (updated_at, award_id)
    WHERE price_notice_eligible;

CREATE TABLE billing_founding_price_notice_state (
    award_id TEXT PRIMARY KEY REFERENCES billing_founding_awards (award_id) ON DELETE RESTRICT,
    transition_epoch BIGINT CHECK (transition_epoch IS NULL OR transition_epoch > 0),
    schedule_version BIGINT NOT NULL CHECK (schedule_version > 0),
    payer_kind TEXT NOT NULL CHECK (payer_kind IN ('personal', 'sponsor')),
    payer_id TEXT NOT NULL CHECK (btrim(payer_id) <> ''),
    outcome TEXT NOT NULL CHECK (outcome IN ('queued', 'late_unresolved', 'retired', 'schedule_unresolved')),
    reason_code TEXT CHECK (reason_code IS NULL OR reason_code IN (
        'notice_window_elapsed', 'recipient_bound_exceeded', 'recipient_unavailable',
        'schedule_not_verified', 'subscription_ending'
    )),
    next_reconcile_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((outcome = 'queued' AND reason_code IS NULL)
        OR (outcome <> 'queued' AND reason_code IS NOT NULL)),
    CHECK (transition_epoch IS NOT NULL
        OR (outcome = 'schedule_unresolved' AND reason_code = 'schedule_not_verified'))
);

CREATE INDEX billing_founding_price_notice_state_outcome_idx
    ON billing_founding_price_notice_state (outcome, reason_code, next_reconcile_at);
