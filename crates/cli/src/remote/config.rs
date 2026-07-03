//! Global (secret-free) CLI config: the sync server URL.
//!
//! Resolved by precedence: an explicit `--server` override → the `SOTTO_SERVER` environment
//! variable → the saved `config.toml` → the hosted instance. `login --server <url>` persists it
//! for later commands. Machine mode deliberately skips the hosted fallback (see
//! [`explicit_server_url`]): CI must name its server rather than send a token to a default one.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const SERVER_ENV: &str = "SOTTO_SERVER";

/// The hosted Sotto instance, used when nothing else names a server. Self-hosters override via
/// `login --server <url>`, `SOTTO_SERVER`, or the saved config.
pub const DEFAULT_SERVER: &str = "https://getsotto.co.uk";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub server_url: String,
    /// Origin of the web app, for building share links. Falls back to `server_url` when unset
    /// (same-origin deploy).
    #[serde(default)]
    pub web_url: Option<String>,
}

impl GlobalConfig {
    /// Load the config, or `None` if the file is absent (anything else is a real error).
    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|e| Error::Config(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e.to_string())),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(path, text).map_err(|e| Error::Io(e.to_string()))
    }
}

/// Pure precedence: override → env → configured. Each candidate is trimmed of trailing slashes and
/// dropped if empty, so e.g. an empty `--server ""` falls through to the next source.
fn resolve(
    override_url: Option<&str>,
    env_url: Option<String>,
    configured: Option<String>,
) -> Option<String> {
    fn normalize(s: String) -> Option<String> {
        let trimmed = s.trim().trim_end_matches('/').to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    }
    override_url
        .map(str::to_string)
        .and_then(normalize)
        .or_else(|| env_url.and_then(normalize))
        .or_else(|| configured.and_then(normalize))
}

/// Resolve the server URL: override → `SOTTO_SERVER` → saved config → the hosted instance.
pub fn server_url(override_url: Option<&str>, config_path: &Path) -> Result<String> {
    Ok(explicit_server_url(override_url, config_path)?
        .unwrap_or_else(|| DEFAULT_SERVER.to_string()))
}

/// Resolve an explicitly named server (override → `SOTTO_SERVER` → saved config), with NO hosted
/// fallback — `None` means nothing named one. Machine mode uses this so a misconfigured CI job
/// fails loudly instead of sending its bearer token to a server the token wasn't minted for.
pub fn explicit_server_url(
    override_url: Option<&str>,
    config_path: &Path,
) -> Result<Option<String>> {
    let env_url = std::env::var(SERVER_ENV).ok();
    let configured = GlobalConfig::load_from(config_path)?.map(|c| c.server_url);
    Ok(resolve(override_url, env_url, configured))
}

/// Resolve the web-app origin for share links: the configured `web_url` if set, else the server URL
/// (same-origin deploy).
pub fn web_base(config_path: &Path) -> Result<String> {
    let web = GlobalConfig::load_from(config_path)?.and_then(|c| c.web_url);
    match web
        .map(|w| w.trim_end_matches('/').to_string())
        .filter(|w| !w.is_empty())
    {
        Some(web) => Ok(web),
        None => server_url(None, config_path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = GlobalConfig {
            server_url: "https://api.sotto.dev".into(),
            web_url: Some("https://app.sotto.dev".into()),
        };
        config.save_to(&path).unwrap();
        assert_eq!(GlobalConfig::load_from(&path).unwrap().unwrap(), config);
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GlobalConfig::load_from(&dir.path().join("nope.toml"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolve_precedence_and_normalization() {
        assert_eq!(
            resolve(
                Some("https://a/"),
                Some("https://b".into()),
                Some("https://c".into())
            ),
            Some("https://a".into())
        );
        assert_eq!(
            resolve(None, Some("https://b/".into()), Some("https://c".into())),
            Some("https://b".into())
        );
        assert_eq!(
            resolve(None, None, Some("https://c".into())),
            Some("https://c".into())
        );
        assert_eq!(resolve(None, None, None), None);
        assert_eq!(
            resolve(Some(""), None, Some("https://c".into())),
            Some("https://c".into())
        );
    }

    #[test]
    fn default_server_is_normalized_https() {
        // server_url returns DEFAULT_SERVER verbatim when nothing else resolves, so the constant
        // itself must already satisfy the normalization invariants.
        assert!(DEFAULT_SERVER.starts_with("https://"));
        assert!(!DEFAULT_SERVER.ends_with('/'));
        assert_eq!(DEFAULT_SERVER.trim(), DEFAULT_SERVER);
    }

    #[test]
    fn saved_config_beats_the_hosted_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        GlobalConfig {
            server_url: "https://self.hosted".into(),
            web_url: None,
        }
        .save_to(&path)
        .unwrap();
        // (env-var precedence is covered by the pure `resolve` tests; mutating the real
        // process environment here would race with parallel tests)
        assert_eq!(server_url(None, &path).unwrap(), "https://self.hosted");
        assert_eq!(
            explicit_server_url(None, &path).unwrap(),
            Some("https://self.hosted".into())
        );
    }
}
