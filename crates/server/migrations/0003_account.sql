-- Account crypto material is server-opaque: stored and returned verbatim, never interpreted.
--
-- 0001 declared `kdf_params` as JSONB, but the server never queries it — the salt and Argon2
-- parameters live inside, serialized by the client. Make it opaque bytes like the other key
-- material. Change the column type in place (no DROP, so anything depending on the column is
-- preserved) and fail loudly rather than silently discarding data if this ever runs on a DB that
-- already holds values (no account has been initialized yet, so every `kdf_params` is NULL).

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM users WHERE kdf_params IS NOT NULL) THEN
        RAISE EXCEPTION 'kdf_params already has data; refusing to change its type (would lose data)';
    END IF;
END $$;

ALTER TABLE users ALTER COLUMN kdf_params TYPE BYTEA USING NULL::bytea;
