//! Anonymous, opt-out version-ping telemetry: the sending task every server runs by default,
//! and the ingest endpoint only the hosted instance enables.
//!
//! **The entire payload is [`Ping`]** - a random instance UUID (generated once, stored in
//! Postgres, derived from nothing), the server version, and compile-time OS/arch. The ingest
//! side stores no IPs, no hostnames, no org/member/secret counts, and no usage events; the CLI,
//! web client, and WASM never send anything. The README's Telemetry section documents the
//! payload and links here, so the claim is checkable against this file.
//!
//! Opt out with `SOTTO_TELEMETRY=off` or the cross-tool `DO_NOT_TRACK=1`
//! (<https://consoledonottrack.com>): the task is then never spawned, so no request is ever
//! made. In return the ping's response names the latest release, which is logged when this
//! server is behind - for a secrets server, knowing you're unpatched is worth one line a day.
//!
//! Ingest ships dark like OAuth/billing: `POST /telemetry/v1/ping` returns 503 unless
//! `SOTTO_TELEMETRY_INGEST=1` (set only on the hosted deployment).

use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::TelemetryConfig;
use crate::error::{Error, Result};
use crate::state::AppState;

/// This build's version - what the ping reports, and what ingest returns as `latest_version`
/// (the hosted instance runs the newest release, so its own version is the fleet's reference).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Base startup delay before the first ping; the per-instance offset stretches it to a 10-20
/// minute window, so crash-looping instances never register and a fleet's pings don't align.
const INITIAL_DELAY_SECS: u64 = 600;
/// One ping (and, on the ingest host, one purge pass) per day.
const PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
/// Ingest-side cap per payload field - the real payload's fields are all far shorter.
const MAX_FIELD_LEN: usize = 64;

pub fn router() -> Router<AppState> {
    Router::new().route("/telemetry/v1/ping", post(ingest))
}

/// The complete telemetry payload. Adding a field here is a privacy-policy change: it must be
/// reflected in the README's Telemetry section and SECURITY.md in the same commit.
#[derive(Debug, Serialize, Deserialize)]
struct Ping {
    /// Random UUID from `telemetry_instance` - the only identifier, derived from nothing.
    instance_id: String,
    /// Sender's `CARGO_PKG_VERSION`.
    version: String,
    /// Sender's `std::env::consts::OS` (e.g. `linux`).
    os: String,
    /// Sender's `std::env::consts::ARCH` (e.g. `x86_64`).
    arch: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PingResponse {
    latest_version: String,
}

/// `POST /telemetry/v1/ping` - record that an instance exists (hosted only; 503 elsewhere).
///
/// The body arrives as a `Result` so a `Json` rejection can't preempt the ships-dark gate: a
/// bare `Json<Ping>` extractor would answer malformed posts with its own 400/415/422 *before*
/// this function runs, and a dark instance must answer 503 unconditionally.
async fn ingest(
    State(state): State<AppState>,
    payload: std::result::Result<Json<Ping>, JsonRejection>,
) -> Result<Json<PingResponse>> {
    if !state.telemetry_ingest {
        return Err(Error::NotConfigured(
            "telemetry ingest is not enabled on this server".into(),
        ));
    }
    let Json(ping) = payload.map_err(|_| Error::BadRequest("body must be a JSON ping".into()))?;

    // Requiring a parseable UUID (re-serialised, which lowercases) keeps arbitrary junk - and
    // therefore any smuggled PII - out of the table, and dedupes case variants of one id.
    let instance_id = Uuid::parse_str(ping.instance_id.trim())
        .map_err(|_| Error::BadRequest("instance_id must be a UUID".into()))?
        .to_string();
    for (name, value) in [
        ("version", &ping.version),
        ("os", &ping.os),
        ("arch", &ping.arch),
    ] {
        if value.is_empty() || value.len() > MAX_FIELD_LEN {
            return Err(Error::BadRequest(format!(
                "{name} must be 1-{MAX_FIELD_LEN} bytes"
            )));
        }
    }

    sqlx::query(
        "INSERT INTO telemetry_pings (instance_id, version, os, arch) VALUES ($1, $2, $3, $4)
         ON CONFLICT (instance_id) DO UPDATE
         SET version = EXCLUDED.version, os = EXCLUDED.os, arch = EXCLUDED.arch,
             last_seen = now()",
    )
    .bind(&instance_id)
    .bind(&ping.version)
    .bind(&ping.os)
    .bind(&ping.arch)
    .execute(&state.pool)
    .await?;

    Ok(Json(PingResponse {
        latest_version: VERSION.to_string(),
    }))
}

/// Start this instance's background telemetry work: the daily ping (the default), nothing at
/// all (opted out), or - on the ingest host - the daily retention purge.
pub fn spawn(pool: PgPool, config: TelemetryConfig) {
    if config.ingest_enabled {
        // The ingest host doesn't ping itself - it *is* the census, and its own version is the
        // fleet's update reference. It owns retention instead.
        tokio::spawn(purge_loop(pool));
        return;
    }
    if !config.ping_enabled {
        // Opted out: no task, no HTTP client, no request - ever.
        return;
    }
    tokio::spawn(ping_loop(pool, config.endpoint));
}

async fn ping_loop(pool: PgPool, endpoint: String) {
    let Ok(instance_id) = instance_id(&pool).await else {
        // No stored id → no ping. Never invent an unstored one: a restart would mint another
        // and double-count this instance.
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return;
    };

    // Deterministic per-instance offset: spreads a fleet's pings across the window without an
    // RNG dependency, and keeps this instance's daily slot stable across restarts.
    let offset = instance_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    tokio::time::sleep(Duration::from_secs(
        INITIAL_DELAY_SECS + offset % INITIAL_DELAY_SECS,
    ))
    .await;

    loop {
        // Failures are dropped silently: an egress-blocked self-host is a normal, supported
        // configuration, not a condition to spam the logs over.
        if let Ok(response) = ping_once(&client, &endpoint, &instance_id).await {
            if is_newer(&response.latest_version, VERSION) {
                println!(
                    "sotto-server {} is available (running {VERSION}) - \
                     https://github.com/getsotto/sotto/releases",
                    response.latest_version
                );
            }
        }
        tokio::time::sleep(PERIOD).await;
    }
}

async fn ping_once(
    client: &reqwest::Client,
    endpoint: &str,
    instance_id: &str,
) -> std::result::Result<PingResponse, reqwest::Error> {
    client
        .post(endpoint)
        .json(&Ping {
            instance_id: instance_id.to_string(),
            version: VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        })
        .send()
        .await?
        .error_for_status()?
        .json::<PingResponse>()
        .await
}

/// The instance's stable random id, created on first use. `ON CONFLICT DO NOTHING` plus the
/// re-select makes concurrent first boots against one database converge on a single id.
async fn instance_id(pool: &PgPool) -> sqlx::Result<String> {
    sqlx::query(
        "INSERT INTO telemetry_instance (singleton, instance_id) VALUES (TRUE, $1)
         ON CONFLICT (singleton) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .execute(pool)
    .await?;
    sqlx::query_scalar("SELECT instance_id FROM telemetry_instance")
        .fetch_one(pool)
        .await
}

/// Hosted-side retention: drop instances idle for 12 months, once a day.
async fn purge_loop(pool: PgPool) {
    loop {
        if let Err(e) = sqlx::query(
            "DELETE FROM telemetry_pings WHERE last_seen < now() - interval '12 months'",
        )
        .execute(&pool)
        .await
        {
            eprintln!("telemetry purge failed: {e}");
        }
        tokio::time::sleep(PERIOD).await;
    }
}

/// `true` when `candidate` is a strictly newer `x.y.z` than `current`. Anything that isn't a
/// plain `x.y.z` (pre-releases, junk from a hostile endpoint) compares as not-newer: the only
/// consequence of this function is one log line, so failing quiet is the right shape.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (version_triple(candidate), version_triple(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The privacy contract: the wire payload is exactly the four documented fields. If this
    /// test changes, README's Telemetry section and SECURITY.md must change with it.
    #[test]
    fn payload_is_exactly_the_documented_four_fields() {
        let ping = Ping {
            instance_id: "0d0972a6-4b6a-4b8e-9c0a-2f4a0e6b1c9d".into(),
            version: "0.2.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
        };
        assert_eq!(
            serde_json::to_value(&ping).unwrap(),
            serde_json::json!({
                "instance_id": "0d0972a6-4b6a-4b8e-9c0a-2f4a0e6b1c9d",
                "version": "0.2.0",
                "os": "linux",
                "arch": "x86_64",
            })
        );
    }

    #[test]
    fn update_notice_fires_only_on_strictly_newer_plain_semver() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("v0.2.1", "0.2.0")); // tolerate a tag-style v prefix
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-rc1", "0.2.0")); // pre-releases stay quiet
        assert!(!is_newer("newest!!", "0.2.0")); // hostile endpoint can't spoof the log line
        assert!(!is_newer("1.2.3.4", "0.2.0"));
    }
}
