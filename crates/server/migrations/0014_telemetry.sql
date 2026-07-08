-- Telemetry (anonymous, opt-out): the instance's own random id, and — hosted only — the ingest
-- table that counts pings from the fleet.
--
-- `telemetry_instance` is a single row holding a random UUID (TEXT, like every id in this
-- schema) generated on first boot. It is the ONLY identifier the daily version ping ever
-- carries — never hardware, hostname, or account data — so wiping it (or the database) makes
-- the instance a brand-new anonymous counter. See crates/server/src/telemetry.rs and the
-- README's Telemetry section.
--
-- `telemetry_pings` exists on every database (migrations are unconditional) but only an ingest
-- host (`SOTTO_TELEMETRY_INGEST=1`, i.e. the hosted instance) ever writes to it. One row per
-- reporting instance; the metric is "distinct instances seen in the last N days". No IPs and no
-- derived location — the row is exactly the ping payload plus timestamps — and rows idle for
-- 12 months are purged daily.

CREATE TABLE IF NOT EXISTS telemetry_instance (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    instance_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS telemetry_pings (
    instance_id TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    os TEXT NOT NULL,
    arch TEXT NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now()
);
