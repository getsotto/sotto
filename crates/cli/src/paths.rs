//! Filesystem locations for the local store.
//!
//! The OS data directory is resolved from platform conventions directly (no `directories` crate -
//! that pulls an MPL-2.0 transitive dependency, and the logic here is small).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Overrides the data directory (handy for tests and power users).
const DATA_DIR_ENV: &str = "SOTTO_DATA_DIR";

/// The directory holding the local store: `SOTTO_DATA_DIR` if set, else the OS data directory.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        // An empty override would become `PathBuf::from("")`, which resolves to the current
        // directory - almost certainly a misconfiguration, and the wrong place for a secret store.
        if dir.is_empty() {
            return Err(Error::Input(format!("{DATA_DIR_ENV} is set but empty")));
        }
        return Ok(PathBuf::from(dir));
    }
    platform_data_dir().ok_or_else(|| Error::Io("could not determine a data directory".into()))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/sotto"))
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("sotto"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_data_dir() -> Option<PathBuf> {
    // XDG Base Directory: $XDG_DATA_HOME (if an absolute path), else ~/.local/share. The spec
    // says relative (and empty) values must be ignored - `is_absolute` covers both.
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join("sotto"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/sotto"))
}

/// The SQLite store path inside a given data directory.
pub fn store_file(data_dir: &Path) -> PathBuf {
    data_dir.join("store.db")
}

/// The resolved store path.
pub fn store_path() -> Result<PathBuf> {
    Ok(store_file(&data_dir()?))
}

/// The global (secret-free) config path inside a given data directory.
pub fn config_file(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

/// The resolved global config path.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_file(&data_dir()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_file_is_under_data_dir() {
        assert_eq!(store_file(Path::new("/data")), Path::new("/data/store.db"));
    }
}
