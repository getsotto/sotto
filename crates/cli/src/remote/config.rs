//! Global (secret-free) CLI config: the sync server URL.
//!
//! Resolved by precedence: an explicit `--server` override → the `SOTTO_SERVER` environment
//! variable → the saved `config.toml`. `login --server <url>` persists it for later commands.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const SERVER_ENV: &str = "SOTTO_SERVER";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub server_url: String,
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

/// Resolve the server URL (override → `SOTTO_SERVER` → saved config), or an actionable error.
pub fn server_url(override_url: Option<&str>, config_path: &Path) -> Result<String> {
    let env_url = std::env::var(SERVER_ENV).ok();
    let configured = GlobalConfig::load_from(config_path)?.map(|c| c.server_url);
    resolve(override_url, env_url, configured).ok_or_else(|| {
        Error::Input("no server configured; run `sotto login --server <url>`".into())
    })
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
}
