-- Index the OAuth login-state table by `created_at`.
--
-- `auth::routes::login` opportunistically clears expired login states on every unauthenticated
-- `GET /auth/github/login` with `DELETE FROM oauth_logins WHERE created_at < now() - interval …`.
-- Without an index that is a sequential scan, so a burst of logins (each call re-scanning the
-- table) degrades super-linearly. Edge rate limiting (deploy/Caddyfile) is the primary control on
-- that endpoint; this index is the server-side backstop that keeps the sweep cheap even if a flood
-- from many source IPs slips past a per-IP limit.
CREATE INDEX IF NOT EXISTS oauth_logins_created_at_idx ON oauth_logins (created_at);
