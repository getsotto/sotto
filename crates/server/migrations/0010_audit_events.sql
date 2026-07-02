-- Org-scoped audit log (M6 PR4): an append-only record of team state changes.
--
-- Every server-observable *state change* in an organization lands here — membership, grants,
-- rotations, machine tokens, batch secret writes, account resets. Reads are never logged. The
-- server stores only ids and metadata (it never has names or values), so the log is as
-- zero-knowledge as the rest of the schema.
--
-- Append-only by construction: rows are only ever INSERTed (no update/delete endpoints), and
-- `id BIGSERIAL` gives a stable order. `actor`/`target` are plain TEXT — deliberately NOT
-- foreign keys — so history survives a user row's deletion. Deleting the org cascades its log
-- away with everything else (a deliberate, owner-only act).

CREATE TABLE IF NOT EXISTS audit_events (
    id         BIGSERIAL PRIMARY KEY,
    org_id     TEXT NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    -- Who did it (a user id; machine-token actions record the acting admin, not the machine).
    actor      TEXT NOT NULL,
    -- Stable dotted action name, e.g. `member.removed`, `env.rotated`, `secrets.written`.
    action     TEXT NOT NULL,
    -- The acted-on user or token id, when the action has one.
    target     TEXT,
    -- The affected environment, when the action has one.
    env_id     TEXT,
    -- Small human-readable context (a role, a change count) — metadata only, never secret material.
    detail     TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The one read path: an org's events, newest first.
CREATE INDEX IF NOT EXISTS audit_events_org_idx ON audit_events (org_id, id DESC);
