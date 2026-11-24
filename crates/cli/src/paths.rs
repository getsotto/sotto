//! Filesystem locations for the local store.
//!
//! The OS data directory is resolved from platform conventions directly (no `directories` crate -
//! that pulls an MPL-2.0 transitive dependency, and the logic here is small).

use std::ffi::OsString;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Overrides the data directory (handy for tests and power users).
const DATA_DIR_ENV: &str = "SOTTO_DATA_DIR";

/// The directory holding the local store: `SOTTO_DATA_DIR` if set, else the OS data directory.
pub fn data_dir() -> Result<PathBuf> {
    resolve_data_dir(std::env::var_os(DATA_DIR_ENV), platform_data_dir)
}

/// The decision logic behind [`data_dir`], with its inputs passed in so tests need not touch the
/// process environment. The platform default is a closure so it is only consulted when there is
/// no override.
fn resolve_data_dir(
    override_dir: Option<OsString>,
    platform_default: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        // An empty override would become `PathBuf::from("")`, which resolves to the current
        // directory - almost certainly a misconfiguration, and the wrong place for a secret store.
        if dir.is_empty() {
            return Err(Error::Input(format!("{DATA_DIR_ENV} is set but empty")));
        }
        return Ok(PathBuf::from(dir));
    }
    platform_default().ok_or_else(|| Error::Io("could not determine a data directory".into()))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    macos_data_dir(std::env::var_os("HOME"))
}

#[cfg(target_os = "macos")]
fn macos_data_dir(home: Option<OsString>) -> Option<PathBuf> {
    Some(PathBuf::from(home?).join("Library/Application Support/sotto"))
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> Option<PathBuf> {
    windows_data_dir(std::env::var_os("APPDATA"))
}

#[cfg(target_os = "windows")]
fn windows_data_dir(appdata: Option<OsString>) -> Option<PathBuf> {
    Some(PathBuf::from(appdata?).join("sotto"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_data_dir() -> Option<PathBuf> {
    xdg_data_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn xdg_data_dir(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // XDG Base Directory: $XDG_DATA_HOME (if an absolute path), else ~/.local/share. The spec
    // says relative (and empty) values must be ignored - `is_absolute` covers both.
    if let Some(xdg) = xdg {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join("sotto"));
        }
    }
    Some(PathBuf::from(home?).join(".local/share/sotto"))
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

/// The themes directory inside a given data directory.
pub fn themes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("themes")
}

/// The resolved themes directory.
pub fn themes_path() -> Result<PathBuf> {
    Ok(themes_dir(&data_dir()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_file_is_under_data_dir() {
        assert_eq!(store_file(Path::new("/data")), Path::new("/data/store.db"));
    }

    #[test]
    fn themes_dir_is_under_data_dir() {
        assert_eq!(themes_dir(Path::new("/data")), Path::new("/data/themes"));
    }

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn override_wins_without_consulting_platform_defaults() {
        let got = resolve_data_dir(os("/tmp/my sotto data"), || {
            unreachable!("platform default must not be consulted")
        })
        .unwrap();
        assert_eq!(got, PathBuf::from("/tmp/my sotto data"));
    }

    #[test]
    fn empty_override_is_an_input_error() {
        let err = resolve_data_dir(os(""), || Some(PathBuf::from("/fallback"))).unwrap_err();
        assert!(matches!(err, Error::Input(_)), "got {err:?}");
    }

    #[test]
    fn absent_override_uses_platform_default() {
        let got = resolve_data_dir(None, || Some(PathBuf::from("/platform"))).unwrap();
        assert_eq!(got, PathBuf::from("/platform"));
    }

    #[test]
    fn absent_override_and_no_platform_default_is_an_io_error() {
        let err = resolve_data_dir(None, || None).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn override_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![b'/', b't', 0xff, b'x']);
        let got = resolve_data_dir(Some(raw.clone()), || None).unwrap();
        assert_eq!(got.into_os_string(), raw);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_home_library() {
        assert_eq!(
            macos_data_dir(os("/Users/a b")),
            Some(PathBuf::from("/Users/a b").join("Library/Application Support/sotto"))
        );
        assert_eq!(macos_data_dir(None), None);

        let err = resolve_data_dir(None, || macos_data_dir(None)).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_uses_appdata() {
        assert_eq!(
            windows_data_dir(os(r"C:\Users\a\AppData\Roaming")),
            Some(PathBuf::from(r"C:\Users\a\AppData\Roaming").join("sotto"))
        );
        assert_eq!(windows_data_dir(None), None);

        let err = resolve_data_dir(None, || windows_data_dir(None)).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    mod xdg {
        use super::*;

        #[test]
        fn absolute_xdg_wins_over_home() {
            assert_eq!(
                xdg_data_dir(os("/xdg/data"), os("/home/u")),
                Some(PathBuf::from("/xdg/data").join("sotto"))
            );
        }

        #[test]
        fn absolute_xdg_works_without_home() {
            assert_eq!(
                xdg_data_dir(os("/xdg/data"), None),
                Some(PathBuf::from("/xdg/data").join("sotto"))
            );
        }

        #[test]
        fn relative_empty_or_absent_xdg_falls_back_to_home() {
            let expected = Some(PathBuf::from("/home/u").join(".local/share/sotto"));
            for xdg in [os("relative/dir"), os("~/data"), os(""), None] {
                assert_eq!(xdg_data_dir(xdg, os("/home/u")), expected);
            }
        }

        #[test]
        fn missing_variables_surface_as_io_error_through_resolver() {
            let err = resolve_data_dir(None, || xdg_data_dir(None, None)).unwrap_err();
            assert!(matches!(err, Error::Io(_)), "got {err:?}");
        }

        #[test]
        fn unusable_xdg_and_no_home_is_none() {
            for xdg in [os("relative/dir"), os("~/data"), os(""), None] {
                assert_eq!(xdg_data_dir(xdg, None), None);
            }
        }
    }
}
