-- Personal billing events use a user scope because the organisation audit log is keyed by
-- organisations and cannot safely store a user's settlement under a foreign key it does not own.
CREATE TABLE billing_personal_events (
    id             BIGSERIAL PRIMARY KEY,
    user_id        TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    operation_id   TEXT REFERENCES billing_operations (operation_id) ON DELETE RESTRICT,
    action         TEXT NOT NULL,
    detail         TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (btrim(action) <> '')
);

CREATE INDEX billing_personal_events_user_idx
    ON billing_personal_events (user_id, id DESC);
