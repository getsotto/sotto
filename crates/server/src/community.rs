//! Public GitHub community snapshot for the marketing page.
//!
//! `GET /community` is unauthenticated. The bundled Caddyfile puts it in the same per-IP
//! rate-limit zone as the other public routes; the handler itself does not rate-limit, so a
//! deployment that is not using that Caddyfile is unlimited. The browser never talks to
//! `api.github.com`: a strict `connect-src 'self'` CSP forbids it, so this process fetches and
//! caches the numbers and the landing page reads them same-origin.
//!
//! Talking to GitHub ships dark: `SOTTO_COMMUNITY=1` enables the live source (the hosted
//! marketing instance). Everywhere else the route returns 503 and the landing page hides the
//! counts, so a self-hosted server never calls `api.github.com`.
//!
//! The entire JSON body is [`Snapshot`] - stars, forks, contributor logins, and the repo URL.
//! Adding a field is a contract change: the unit test in this file pins the keys. Avatars are
//! omitted on purpose (`img-src 'self'`, and Sordino does not put imagery on the landing page).
//!
//! The cache is in-memory and per-process. Concurrent misses share one in-flight refresh so a
//! burst of landing-page traffic cannot stampede GitHub. A GitHub outage with a warm cache serves
//! the stale snapshot and backs off before retrying; a cold cache and a failed fetch is 502, and
//! the landing page hides the counts.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// How long a successful GitHub fetch is reused before the next request goes upstream.
const TTL: Duration = Duration::from_secs(60 * 60);
/// How long a failed fetch suppresses retries (stale data is still served).
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);
/// Bound the GitHub round-trip so a hung upstream cannot stall the landing page.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);
const REPO_API: &str = "https://api.github.com/repos/getsotto/sotto";
const CONTRIBUTORS_API: &str =
    "https://api.github.com/repos/getsotto/sotto/contributors?per_page=20";
/// Cap what we serialise even if GitHub returns more.
const MAX_CONTRIBUTORS: usize = 20;

/// The complete `/community` payload. Adding a field here must update the pinning test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub stars: u32,
    pub forks: u32,
    pub repo_url: String,
    pub contributors: Vec<Contributor>,
}

/// One GitHub user who has a commit on the repo. `html_url` is their profile, not an avatar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Contributor {
    pub login: String,
    pub html_url: String,
    pub contributions: u32,
}

/// Cloneable handle: a source plus the process-local cache. Production uses [`Handle::live`];
/// tests inject a scripted source so this module never touches the network.
#[derive(Clone)]
pub struct Handle {
    source: Source,
    state: Arc<Mutex<CacheState>>,
    /// Serialises refreshes so concurrent misses share one upstream round-trip.
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
    ttl: Duration,
    negative_ttl: Duration,
    /// Test-only pause before talking to [`Source`], so two requests can overlap a miss.
    fetch_delay: Duration,
}

struct CacheState {
    entry: Option<Cached>,
    /// Do not fetch again until this instant (set after a failure).
    retry_at: Option<Instant>,
}

#[derive(Clone)]
struct Cached {
    at: Instant,
    snapshot: Snapshot,
}

#[derive(Clone)]
enum Source {
    Live { client: reqwest::Client },
    Sequence(Arc<Mutex<VecDeque<Result<Snapshot, String>>>>),
    Disabled,
}

impl Handle {
    /// Production source: GitHub's public repo API, unauthenticated, 10s timeout.
    pub fn live() -> Self {
        let client = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .user_agent("sotto-server")
            .build()
            .expect("reqwest client with static config builds");
        Self::new(Source::Live { client }, TTL, NEGATIVE_TTL, Duration::ZERO)
    }

    /// No GitHub calls. Production uses this unless `SOTTO_COMMUNITY=1`.
    pub fn disabled() -> Self {
        Self::new(Source::Disabled, TTL, NEGATIVE_TTL, Duration::ZERO)
    }

    fn enabled(&self) -> bool {
        !matches!(self.source, Source::Disabled)
    }

    /// Scripted source for tests. Each call to GitHub is a pop from the front of `responses`.
    pub fn sequence(responses: Vec<Result<Snapshot, String>>) -> Self {
        Self::sequence_with_ttl(responses, TTL)
    }

    pub fn sequence_with_ttl(responses: Vec<Result<Snapshot, String>>, ttl: Duration) -> Self {
        Self::new(
            Source::Sequence(Arc::new(Mutex::new(VecDeque::from(responses)))),
            ttl,
            NEGATIVE_TTL,
            Duration::ZERO,
        )
    }

    pub fn sequence_with_ttl_and_backoff(
        responses: Vec<Result<Snapshot, String>>,
        ttl: Duration,
        negative_ttl: Duration,
        fetch_delay: Duration,
    ) -> Self {
        Self::new(
            Source::Sequence(Arc::new(Mutex::new(VecDeque::from(responses)))),
            ttl,
            negative_ttl,
            fetch_delay,
        )
    }

    fn new(source: Source, ttl: Duration, negative_ttl: Duration, fetch_delay: Duration) -> Self {
        Self {
            source,
            state: Arc::new(Mutex::new(CacheState {
                entry: None,
                retry_at: None,
            })),
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
            ttl,
            negative_ttl,
            fetch_delay,
        }
    }

    async fn snapshot(&self) -> Option<Snapshot> {
        if !self.enabled() {
            return None;
        }
        if let Some(hit) = self.fresh() {
            return Some(hit);
        }
        if self.in_backoff() {
            return self.stale();
        }
        let _refresh = self.fetch_lock.lock().await;
        if let Some(hit) = self.fresh() {
            return Some(hit);
        }
        if self.in_backoff() {
            return self.stale();
        }
        if !self.fetch_delay.is_zero() {
            tokio::time::sleep(self.fetch_delay).await;
        }
        match self.source.fetch().await {
            Ok(snapshot) => {
                self.store(snapshot.clone());
                Some(snapshot)
            }
            Err(e) => {
                eprintln!("community: github fetch failed: {e}");
                self.note_failure();
                self.stale()
            }
        }
    }

    fn fresh(&self) -> Option<Snapshot> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cached = guard.entry.as_ref()?;
        (cached.at.elapsed() < self.ttl).then(|| cached.snapshot.clone())
    }

    fn in_backoff(&self) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.retry_at.is_some_and(|at| Instant::now() < at)
    }

    fn stale(&self) -> Option<Snapshot> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry
            .as_ref()
            .map(|c| c.snapshot.clone())
    }

    fn store(&self, snapshot: Snapshot) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.entry = Some(Cached {
            at: Instant::now(),
            snapshot,
        });
        guard.retry_at = None;
    }

    fn note_failure(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.retry_at = Some(Instant::now() + self.negative_ttl);
    }
}

impl Source {
    async fn fetch(&self) -> Result<Snapshot, String> {
        match self {
            Self::Live { client } => fetch_github(client).await,
            Self::Disabled => Err("community github is not enabled".into()),
            Self::Sequence(queue) => queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| Err("community source exhausted".into())),
        }
    }
}

async fn fetch_github(client: &reqwest::Client) -> Result<Snapshot, String> {
    // Both calls share the client timeout; running them together keeps the whole refresh inside
    // that bound instead of stacking two sequential 10s waits.
    let (repo, raw) = tokio::try_join!(
        get_github::<GithubRepo>(client, REPO_API),
        get_github::<Vec<GithubContributor>>(client, CONTRIBUTORS_API),
    )?;
    let contributors = raw
        .into_iter()
        .filter(|c| c.usable())
        .take(MAX_CONTRIBUTORS)
        .map(|c| Contributor {
            login: c.login,
            html_url: c.html_url,
            contributions: c.contributions,
        })
        .collect();
    Ok(Snapshot {
        stars: repo.stargazers_count,
        forks: repo.forks_count,
        repo_url: repo.html_url,
        contributors,
    })
}

async fn get_github<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct GithubRepo {
    stargazers_count: u32,
    forks_count: u32,
    html_url: String,
}

#[derive(Deserialize)]
struct GithubContributor {
    #[serde(default)]
    login: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    contributions: u32,
    #[serde(default)]
    r#type: String,
}

impl GithubContributor {
    fn usable(&self) -> bool {
        !self.login.is_empty()
            && !self.login.ends_with("[bot]")
            && (self.r#type.is_empty() || self.r#type == "User")
    }
}

/// Production router: live GitHub when `SOTTO_COMMUNITY=1`, otherwise dark.
pub fn router() -> Router<AppState> {
    if community_github_enabled() {
        router_with(Handle::live())
    } else {
        router_with(Handle::disabled())
    }
}

fn community_github_enabled() -> bool {
    std::env::var("SOTTO_COMMUNITY")
        .ok()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Test seam: the same route with an injected handle, still typed as `Router<AppState>` so it
/// merges into the real app. HTTP tests that do not need `AppState` use [`router_standalone`].
pub fn router_with(handle: Handle) -> Router<AppState> {
    Router::new()
        .route("/community", get(community))
        .layer(Extension(handle))
}

/// HTTP tests drive this so they do not have to construct an [`AppState`] (or a database).
pub fn router_standalone(handle: Handle) -> axum::Router {
    Router::new()
        .route("/community", get(community))
        .layer(Extension(handle))
}

/// `GET /community` - cached GitHub stars, forks, and contributor logins.
async fn community(Extension(handle): Extension<Handle>) -> Result<Json<Snapshot>, StatusCode> {
    if !handle.enabled() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    handle
        .snapshot()
        .await
        .map(Json)
        .ok_or(StatusCode::BAD_GATEWAY)
}

#[cfg(test)]
mod pin {
    use super::*;
    use std::collections::BTreeSet;

    fn sample() -> Snapshot {
        Snapshot {
            stars: 12,
            forks: 3,
            repo_url: "https://github.com/getsotto/sotto".into(),
            contributors: vec![Contributor {
                login: "alice".into(),
                html_url: "https://github.com/alice".into(),
                contributions: 8,
            }],
        }
    }

    #[test]
    fn snapshot_serialises_to_exactly_these_fields() {
        let json = serde_json::to_value(sample()).unwrap();
        let keys: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["contributors", "forks", "repo_url", "stars"])
        );
        let contrib = &json["contributors"][0];
        let contrib_keys: BTreeSet<&str> = contrib
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            contrib_keys,
            BTreeSet::from(["contributions", "html_url", "login"])
        );
    }

    #[test]
    fn bots_and_anonymous_entries_are_dropped() {
        assert!(!GithubContributor {
            login: "dependabot[bot]".into(),
            html_url: "https://github.com/apps/dependabot".into(),
            contributions: 4,
            r#type: "Bot".into(),
        }
        .usable());
        assert!(!GithubContributor {
            login: String::new(),
            html_url: String::new(),
            contributions: 1,
            r#type: "Anonymous".into(),
        }
        .usable());
        assert!(GithubContributor {
            login: "alice".into(),
            html_url: "https://github.com/alice".into(),
            contributions: 8,
            r#type: "User".into(),
        }
        .usable());
    }
}
