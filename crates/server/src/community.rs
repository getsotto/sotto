//! Public GitHub community snapshot for the marketing page.
//!
//! `GET /community` is unauthenticated (Caddy rate-limits it with the other public routes). The
//! browser never talks to `api.github.com`: a strict `connect-src 'self'` CSP forbids it, so this
//! process fetches and caches the numbers and the landing page reads them same-origin.
//!
//! The entire JSON body is [`Snapshot`] - stars, forks, contributor logins, and the repo URL.
//! Adding a field is a contract change: the unit test in this file pins the keys. Avatars are
//! omitted on purpose (`img-src 'self'`, and Sordino does not put imagery on the landing page).
//!
//! The cache is in-memory and per-process. A GitHub outage with a warm cache serves the stale
//! snapshot; a cold cache and a failed fetch is 502, and the landing page hides the counts.

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
    cache: Arc<Mutex<Option<Cached>>>,
    ttl: Duration,
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
        Self {
            source: Source::Live { client },
            cache: Arc::new(Mutex::new(None)),
            ttl: TTL,
        }
    }

    /// Scripted source for tests. Each call to GitHub is a pop from the front of `responses`.
    pub fn sequence(responses: Vec<Result<Snapshot, String>>) -> Self {
        Self::sequence_with_ttl(responses, TTL)
    }

    pub fn sequence_with_ttl(responses: Vec<Result<Snapshot, String>>, ttl: Duration) -> Self {
        Self {
            source: Source::Sequence(Arc::new(Mutex::new(VecDeque::from(responses)))),
            cache: Arc::new(Mutex::new(None)),
            ttl,
        }
    }

    async fn snapshot(&self) -> Option<Snapshot> {
        if let Some(hit) = self.fresh() {
            return Some(hit);
        }
        match self.source.fetch().await {
            Ok(snapshot) => {
                self.store(snapshot.clone());
                Some(snapshot)
            }
            Err(_) => self.stale(),
        }
    }

    fn fresh(&self) -> Option<Snapshot> {
        let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let cached = guard.as_ref()?;
        (cached.at.elapsed() < self.ttl).then(|| cached.snapshot.clone())
    }

    fn stale(&self) -> Option<Snapshot> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.snapshot.clone())
    }

    fn store(&self, snapshot: Snapshot) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(Cached {
            at: Instant::now(),
            snapshot,
        });
    }
}

impl Source {
    async fn fetch(&self) -> Result<Snapshot, String> {
        match self {
            Self::Live { client } => fetch_github(client).await,
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

/// Production router: live GitHub, merged into [`crate::app`].
pub fn router() -> Router<AppState> {
    router_with(Handle::live())
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
