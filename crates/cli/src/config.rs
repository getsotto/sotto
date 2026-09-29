//! The committed, secret-free project config (`sotto.toml`).
//!
//! Binds a directory to a local project + default environment so `sotto run`/`get`/… know which
//! secrets to use. Contains **no secrets** - only identifiers - so it's safe to commit. (Forward
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
    /// Owning organisation id, when this project is shared with a team. Absent = personal project.
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
            std::io::ErrorKind::NotFound => Error::NoConfig(path.clone()),
            _ => Error::Io(e.to_string()),
        })?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
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

    #[test]
    fn discover_prefers_nearest_nested_config() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let nested = parent.join("nested project");
        let start = nested.join("src/subdir");
        std::fs::create_dir_all(&start).unwrap();

        let parent_config = Config {
            project_id: "11111111-1111-1111-1111-111111111111".into(),
            project: "parent-api".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let nested_config = Config {
            project_id: "22222222-2222-2222-2222-222222222222".into(),
            project: "nested-api".into(),
            environment: "staging".into(),
            org_id: Some("33333333-3333-3333-3333-333333333333".into()),
        };
        parent_config.save_to(&parent).unwrap();
        nested_config.save_to(&nested).unwrap();

        let (found_config, found_dir) = Config::discover(&start).unwrap();
        assert_eq!(found_config, nested_config);
        assert_eq!(found_dir, nested);
    }

    #[test]
    fn discover_reports_malformed_nearest_config_instead_of_parent() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let nested = parent.join("nested");
        let start = nested.join("src");
        std::fs::create_dir_all(&start).unwrap();
        sample().save_to(&parent).unwrap();
        std::fs::write(nested.join(CONFIG_FILE), "project = [\n").unwrap();

        assert!(matches!(Config::discover(&start), Err(Error::Config(_))));
    }

    #[test]
    fn discover_reports_invalid_utf8_in_nearest_config_instead_of_parent() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let nested = parent.join("nested");
        let start = nested.join("src");
        std::fs::create_dir_all(&start).unwrap();
        sample().save_to(&parent).unwrap();
        std::fs::write(nested.join(CONFIG_FILE), b"\xff\xfe").unwrap();

        assert!(matches!(Config::discover(&start), Err(Error::Io(_))));
    }

    #[test]
    fn discover_uses_parent_config_when_nearest_config_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let nested = parent.join("nested");
        let start = nested.join("src");
        std::fs::create_dir_all(&start).unwrap();
        sample().save_to(&parent).unwrap();
        sample().save_to(&nested).unwrap();
        std::fs::remove_file(nested.join(CONFIG_FILE)).unwrap();

        let (found_config, found_dir) = Config::discover(&start).unwrap();
        assert_eq!(found_config, sample());
        assert_eq!(found_dir, parent);
    }

    #[test]
    fn parse_error_names_parent_discovered_file() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("my project");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = project.join(CONFIG_FILE);
        let broken = "project = [\n";
        std::fs::write(&config_path, broken).unwrap();

        let child = project.join("nested").join("child");
        std::fs::create_dir_all(&child).unwrap();

        let error = Config::discover(&child).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        let rendered = error.to_string();
        let path = config_path.display().to_string();
        assert!(
            matches!(error, Error::Config(message) if message.contains(&path)),
            "{rendered}"
        );
        assert!(
            rendered.contains("TOML parse error at line 1, column 13"),
            "{rendered}"
        );
        assert!(
            !rendered.contains(&child.display().to_string()),
            "{rendered}"
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), broken);
    }
}
