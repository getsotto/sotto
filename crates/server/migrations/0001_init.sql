-- Single-user subset of the data model (teams/memberships/grants/sharing arrive in M5).
--
-- The server is zero-knowledge: names, values, data keys, and vault keys are all stored as opaque
-- ciphertext (`enc_*`). Ids are TEXT and supplied by the client (the vault binds env/secret ids
-- into the AEAD's associated data, so the server must preserve them byte-for-byte).

CREATE TABLE IF NOT EXISTS users (
    id               TEXT PRIMARY KEY,
    oauth_provider   TEXT NOT NULL,
    oauth_subject    TEXT NOT NULL,
    email            TEXT,
    public_key       BYTEA,
    enc_private_keys BYTEA,
    kdf_params       JSONB,
    recovery_blob    BYTEA,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (oauth_provider, oauth_subject)
);

CREATE TABLE IF NOT EXISTS projects (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    enc_name   BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS environments (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    enc_name      BYTEA NOT NULL,
    enc_vault_key BYTEA NOT NULL,
    -- Monotonic per-environment revision: the sync ETag, and the basis for rollback freshness.
    revision      BIGINT NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS secrets (
    id           TEXT PRIMARY KEY,
    env_id       TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    enc_name     BYTEA NOT NULL,
    enc_value    BYTEA NOT NULL,
    enc_data_key BYTEA NOT NULL,
    version      BIGINT NOT NULL,
    deleted_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS secrets_set_updated_at ON secrets;
CREATE TRIGGER secrets_set_updated_at
BEFORE UPDATE ON secrets
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TABLE IF NOT EXISTS secret_versions (
    id           TEXT PRIMARY KEY,
    secret_id    TEXT NOT NULL REFERENCES secrets (id) ON DELETE CASCADE,
    version      BIGINT NOT NULL,
    enc_name     BYTEA NOT NULL,
    enc_value    BYTEA NOT NULL,
    enc_data_key BYTEA NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (secret_id, version)
);

CREATE INDEX IF NOT EXISTS environments_project_id_idx ON environments (project_id);
CREATE INDEX IF NOT EXISTS secrets_env_id_idx ON secrets (env_id);
