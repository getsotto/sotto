//! The committed, secret-free project config (`sotto.toml`).
//!
//! Binds a directory to a local project + default environment so `sotto run`/`get`/… know which
//! secrets to use. Contains **no secrets** — only identifiers — so it's safe to commit. (Forward
//! compatible with M3, where `project_id` becomes a server id.)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The committed config filename.
pub const CONFIG_FILE: &str = "sotto.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Stable local project id (UUID).
    pub project_id: String,
    /// Human-readable project name.
    pub project: String,
    /// Default environment for this directory (e.g. `dev`).
    pub environment: String,
    /// Owning organization id, when this project is shared with a team. Absent = personal project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

impl Config {
    /// Load the config from `dir/sotto.toml`.
    pub fn load_from(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            // A genuinely-absent file is "no config"; anything else (permission denied, invalid
            // UTF-8, …) is a real I/O fault and must not masquerade as a missing config.
            std::io::ErrorKind::NotFound => Error::NoConfig(path),
            _ => Error::Io(e.to_string()),
        })?;
        toml::from_str(&text).map_err(|e| Error::Config(e.to_string()))
    }

    /// Write the config to `dir/sotto.toml`.
    pub fn save_to(&self, dir: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(dir.join(CONFIG_FILE), text).map_err(|e| Error::Io(e.to_string()))
    }

    /// Find the nearest config by walking up from `start` (like `git`).
    pub fn discover(start: &Path) -> Result<(Self, PathBuf)> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            if d.join(CONFIG_FILE).is_file() {
                return Ok((Self::load_from(d)?, d.to_path_buf()));
            }
            dir = d.parent();
        }
        Err(Error::NoConfig(start.join(CONFIG_FILE)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            project_id: "11111111-1111-1111-1111-111111111111".into(),
            project: "acme-api".into(),
            environment: "dev".into(),
            org_id: None,
        }
    }

    #[test]
    fn toml_round_trips() {
        let c = sample();
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn save_then_discover_from_subdir() {
        let root = tempfile::tempdir().unwrap();
        sample().save_to(root.path()).unwrap();
        let sub = root.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();

        let (loaded, found) = Config::discover(&sub).unwrap();
        assert_eq!(loaded, sample());
        assert_eq!(found, root.path());
    }

    #[test]
    fn discover_missing_is_error() {
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            Config::discover(empty.path()),
            Err(Error::NoConfig(_))
        ));
    }
}
