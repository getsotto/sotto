-- Organizations, memberships, and roles — the team-RBAC substrate (M5 PR2).
--
-- An organization groups people (and, from a later PR, projects). Authority lives entirely in
-- `organization_memberships`: a user's `role` in an org decides what they may do. `created_by` is
-- provenance only (audit), not authority — it is `SET NULL` on user deletion so removing the
-- creator never destroys a team other owners still run.
--
-- Zero-knowledge is preserved: `enc_name` is opaque ciphertext, exactly like `projects.enc_name` —
-- the server stores bytes and never learns the plaintext. (In this PR the creator encrypts the name
-- under their own master key; a shared org key so every member can read it arrives with grant
-- sharing in a later PR.)

CREATE TABLE IF NOT EXISTS organizations (
    id         TEXT PRIMARY KEY,
    enc_name   BYTEA NOT NULL,
    created_by TEXT REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS organization_memberships (
    org_id     TEXT NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Enforced in SQL as a backstop to the application's `Role` enum, so a bad write can't slip in.
    role       TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);

-- "Which orgs is this user in?" is the hot lookup (every org-scoped request resolves the caller's
-- role); the PK already indexes "who is in this org?".
CREATE INDEX IF NOT EXISTS organization_memberships_user_idx
    ON organization_memberships (user_id);
