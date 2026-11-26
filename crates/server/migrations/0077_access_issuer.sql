CREATE TABLE access_authorisation_codes (
    code_hash BYTEA PRIMARY KEY,
    registration BYTEA NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    challenge TEXT NOT NULL,
    nonce TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX access_authorisation_codes_expiry ON access_authorisation_codes (expires_at);

CREATE TABLE access_client_assertions (
    assertion_hash BYTEA PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX access_client_assertions_expiry ON access_client_assertions (expires_at);
