-- Account crypto material is server-opaque: stored and returned verbatim, never interpreted.
--
-- 0001 declared `kdf_params` as JSONB, but the server never queries it — the salt and Argon2
-- parameters live inside, serialized by the client. Make it opaque bytes like the other key
-- material. Safe to drop/re-add: no account has been initialized yet (every `kdf_params` is NULL).

ALTER TABLE users DROP COLUMN kdf_params;
ALTER TABLE users ADD COLUMN kdf_params BYTEA;
