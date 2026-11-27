CREATE TABLE access_connection_attempts (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    owner_id TEXT NOT NULL,
    registration BYTEA NOT NULL,
    rotating BOOLEAN NOT NULL,
    code_hash BYTEA UNIQUE,
    staged_hash BYTEA,
    verified BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK (code_hash IS NULL OR staged_hash IS NULL),
    CHECK (NOT verified OR code_hash IS NULL)
);
CREATE INDEX access_connection_attempts_org ON access_connection_attempts(org_id);
CREATE UNIQUE INDEX access_connection_attempts_pending ON access_connection_attempts(org_id)
    WHERE code_hash IS NOT NULL OR staged_hash IS NOT NULL;

CREATE TABLE access_org_connections (
    org_id TEXT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    id TEXT NOT NULL UNIQUE,
    registration BYTEA NOT NULL,
    active_attempt_id TEXT REFERENCES access_connection_attempts(id),
    credential_hash BYTEA UNIQUE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((active_attempt_id IS NULL) = (credential_hash IS NULL))
);

-- Retain the prior deployment binding independently of the organisation row.
CREATE TABLE access_prior_links (
    org_id TEXT NOT NULL,
    registration BYTEA NOT NULL,
    first_connected_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, registration)
);

CREATE TABLE access_deployment_proofs (
    proof_hash BYTEA PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX access_deployment_proofs_expiry ON access_deployment_proofs(expires_at);
