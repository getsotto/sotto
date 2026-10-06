//! Interactive terminal prompt helpers for CLI subcommands.
//!
//! When required arguments are omitted in an interactive terminal (stdin, stdout, and stderr
//! are all TTYs, `TERM` is not dumb, and CI is not active), these functions provide
//! selectable menus using `inquire`.
//!
//! If the session is non-interactive (e.g. piped input/output or CI), missing arguments
//! result in an immediate input error with standard exit code 2 (matching clap).

use std::io::{self, IsTerminal};

use inquire::validator::Validation;
use inquire::{Confirm, CustomType, Select};

use crate::commands::App;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::remote::share::{MAX_TTL_SECONDS, MAX_VIEWS};
use crate::store::Store;
use crate::theme::Theme;

/// Check whether interactive prompting is permissible.
///
/// Prompts require an interactive terminal across stdin, stdout, and stderr,
/// a terminal capable of raw mode and cursor movement (not `TERM=dumb`),
/// and that CI is not active.
pub fn can_prompt() -> bool {
    allowed(
        crate::theme::ci_enabled(std::env::var("CI").ok().as_deref()),
        std::env::var("TERM").is_ok_and(|t| t == "dumb"),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    )
}

/// Preflight check for commands requiring a secret name: if omitted and prompting is disabled,
/// fail immediately without triggering unlock prompts.
pub fn preflight_secret_name(name: Option<&str>) -> Result<()> {
    preflight_secret_name_gate(name, can_prompt())
}

/// Pure testable preflight gate for missing secret name.
pub fn preflight_secret_name_gate(name: Option<&str>, prompt_allowed: bool) -> Result<()> {
    if name.is_none() && !prompt_allowed {
        return Err(Error::MissingArgument(
            "missing required argument <NAME>; provide a secret name or run in an interactive terminal".into(),
        ));
    }
    Ok(())
}

/// Prompt the user to select a secret name from the active environment.
///
/// Returns `Ok(Some(name))` on successful selection, `Ok(None)` if the user cancelled
/// the prompt (e.g. Escape or Ctrl-C), or an error.
pub fn select_secret_key(app: &App, config: &Config, theme: &Theme) -> Result<Option<String>> {
    preflight_secret_name(None)?;

    let names = app.list(config)?;
    if names.is_empty() {
        return Err(Error::NotFound(format!(
            "no secrets found in {}/{}",
            config.project, config.environment
        )));
    }

    let message = format!(
        "Select a secret ({}):",
        theme.bold_accent(&format!("{}/{}", config.project, config.environment))
    );

    match Select::new(&message, names).prompt() {
        Ok(choice) => Ok(Some(choice)),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!("selection failed: {e}"))),
    }
}

/// Prompt the user to select an environment from the project's environments.
///
/// Returns `Ok(Some(env))` on selection, `Ok(None)` if cancelled, or an error.
pub fn select_environment(
    store: &Store,
    project_id: &str,
    theme: &Theme,
) -> Result<Option<String>> {
    if !can_prompt() {
        return Err(Error::MissingArgument(
            "missing required argument <NAME>; provide an environment name or run in an interactive terminal".into(),
        ));
    }

    let mut environments = store.list_environments(project_id)?;
    if environments.is_empty() {
        return Err(Error::NotFound("no environments found for project".into()));
    }
    environments.sort();

    let message = format!(
        "Select environment to use {}:",
        theme.accent("(active environment will be switched)")
    );

    match Select::new(&message, environments).prompt() {
        Ok(choice) => Ok(Some(choice)),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!("selection failed: {e}"))),
    }
}

/// Confirm removal of a secret.
pub fn confirm_removal(name: &str, theme: &Theme) -> Result<bool> {
    if !can_prompt() {
        // In non-interactive mode with name provided, rm does not prompt unless interactive.
        return Ok(true);
    }

    let prompt = format!(
        "Are you sure you want to remove secret {}?",
        theme.error(name)
    );

    match Confirm::new(&prompt).with_default(false).prompt() {
        Ok(confirmed) => Ok(confirmed),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(false),
        Err(e) => Err(Error::Input(format!("prompt failed: {e}"))),
    }
}

/// Options presented to the user when selecting a share link view limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLimitChoice {
    Single,
    Two,
    Three,
    Five,
    Ten,
    Custom,
}

impl ViewLimitChoice {
    /// Return the corresponding view limit count if a fixed preset, or `None` for custom input.
    pub fn views(&self) -> Option<i32> {
        match self {
            Self::Single => Some(1),
            Self::Two => Some(2),
            Self::Three => Some(3),
            Self::Five => Some(5),
            Self::Ten => Some(10),
            Self::Custom => None,
        }
    }
}

impl std::fmt::Display for ViewLimitChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single => write!(f, "1 view (burns after read, default)"),
            Self::Two => write!(f, "2 views"),
            Self::Three => write!(f, "3 views"),
            Self::Five => write!(f, "5 views"),
            Self::Ten => write!(f, "10 views"),
            Self::Custom => write!(f, "Custom view limit..."),
        }
    }
}

/// Options presented to the user when selecting a share link lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifetimePreset {
    NoExpiry,
    OneHour,
    OneDay,
    SevenDays,
    ThirtyDays,
    Custom,
}

impl LifetimePreset {
    /// Return the corresponding TTL in seconds, `Some(None)` for no expiry, or `None` for custom input.
    pub fn ttl_seconds(&self) -> Option<Option<i64>> {
        match self {
            Self::NoExpiry => Some(None),
            Self::OneHour => Some(Some(3600)),
            Self::OneDay => Some(Some(86400)),
            Self::SevenDays => Some(Some(604800)),
            Self::ThirtyDays => Some(Some(MAX_TTL_SECONDS)),
            Self::Custom => None,
        }
    }
}

impl std::fmt::Display for LifetimePreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoExpiry => write!(f, "No expiry (default)"),
            Self::OneHour => write!(f, "1 hour (3600 seconds)"),
            Self::OneDay => write!(f, "1 day (86400 seconds)"),
            Self::SevenDays => write!(f, "7 days (604800 seconds)"),
            Self::ThirtyDays => write!(f, "30 days (2592000 seconds)"),
            Self::Custom => write!(f, "Custom lifetime in seconds..."),
        }
    }
}

/// Validate a candidate view limit number.
pub fn validate_view_limit(views: i32) -> std::result::Result<(), String> {
    if (1..=MAX_VIEWS).contains(&views) {
        Ok(())
    } else {
        Err(format!("view limit must be between 1 and {MAX_VIEWS}"))
    }
}

/// Validate a candidate link lifetime in seconds.
pub fn validate_lifetime_seconds(seconds: i64) -> std::result::Result<(), String> {
    if (1..=MAX_TTL_SECONDS).contains(&seconds) {
        Ok(())
    } else {
        Err(format!(
            "lifetime must be between 1 and {MAX_TTL_SECONDS} seconds"
        ))
    }
}

/// Prompt the user to select or enter a view limit for a share link.
///
/// Returns `Ok(Some(views))` on selection, `Ok(None)` if cancelled, or an error.
pub fn prompt_share_views(theme: &Theme) -> Result<Option<i32>> {
    let options = vec![
        ViewLimitChoice::Single,
        ViewLimitChoice::Two,
        ViewLimitChoice::Three,
        ViewLimitChoice::Five,
        ViewLimitChoice::Ten,
        ViewLimitChoice::Custom,
    ];

    let message = format!(
        "Select view limit {}:",
        theme.accent("(burns after view count reached)")
    );

    match Select::new(&message, options).prompt() {
        Ok(choice) => match choice.views() {
            Some(views) => Ok(Some(views)),
            None => prompt_custom_views(theme),
        },
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!("view limit selection failed: {e}"))),
    }
}

/// Prompt the user to enter a custom view limit (1-100).
pub fn prompt_custom_views(theme: &Theme) -> Result<Option<i32>> {
    let message = format!("Enter custom view limit {}:", theme.accent("(1-100)"));

    match CustomType::<i32>::new(&message)
        .with_default(1)
        .with_error_message("please enter a valid number between 1 and 100")
        .with_validator(|&val: &i32| match validate_view_limit(val) {
            Ok(()) => Ok(Validation::Valid),
            Err(msg) => Ok(Validation::Invalid(msg.into())),
        })
        .prompt()
    {
        Ok(val) => Ok(Some(val)),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!(
            "custom view limit prompt failed: {e}"
        ))),
    }
}

/// Prompt the user to select or enter an expiration lifetime for a share link.
///
/// Returns `Ok(Some(Some(secs)))` on TTL selection, `Ok(Some(None))` for no expiry,
/// `Ok(None)` if cancelled, or an error.
pub fn prompt_share_lifetime(theme: &Theme) -> Result<Option<Option<i64>>> {
    let options = vec![
        LifetimePreset::NoExpiry,
        LifetimePreset::OneHour,
        LifetimePreset::OneDay,
        LifetimePreset::SevenDays,
        LifetimePreset::ThirtyDays,
        LifetimePreset::Custom,
    ];

    let message = format!(
        "Select link lifetime {}:",
        theme.accent("(time until link expires)")
    );

    match Select::new(&message, options).prompt() {
        Ok(preset) => match preset.ttl_seconds() {
            Some(ttl) => Ok(Some(ttl)),
            None => match prompt_custom_lifetime(theme)? {
                Some(secs) => Ok(Some(Some(secs))),
                None => Ok(None),
            },
        },
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!("lifetime selection failed: {e}"))),
    }
}

/// Prompt the user to enter a custom lifetime in seconds (1-2592000).
pub fn prompt_custom_lifetime(theme: &Theme) -> Result<Option<i64>> {
    let message = format!(
        "Enter custom lifetime in seconds {}:",
        theme.accent("(1-2592000)")
    );

    match CustomType::<i64>::new(&message)
        .with_default(3600)
        .with_error_message("please enter a valid number of seconds between 1 and 2592000")
        .with_validator(|&val: &i64| match validate_lifetime_seconds(val) {
            Ok(()) => Ok(Validation::Valid),
            Err(msg) => Ok(Validation::Invalid(msg.into())),
        })
        .prompt()
    {
        Ok(val) => Ok(Some(val)),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(Error::Input(format!("custom lifetime prompt failed: {e}"))),
    }
}

/// Pure testable gate for whether interactive prompts are allowed.
pub fn allowed(
    ci: bool,
    dumb_terminal: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> bool {
    !ci && !dumb_terminal && stdin_tty && stdout_tty && stderr_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_requires_interactive_terminal_on_all_streams() {
        assert!(allowed(false, false, true, true, true));
        assert!(!allowed(true, false, true, true, true));
        assert!(!allowed(false, true, true, true, true));
        assert!(!allowed(false, false, false, true, true));
        assert!(!allowed(false, false, true, false, true));
        assert!(!allowed(false, false, true, true, false));
    }

    #[test]
    fn preflight_secret_name_accepts_present_name() {
        assert!(preflight_secret_name_gate(Some("DATABASE_URL"), false).is_ok());
        assert!(preflight_secret_name_gate(Some("DATABASE_URL"), true).is_ok());
    }

    #[test]
    fn preflight_secret_name_rejects_missing_name_when_prompting_disallowed() {
        let err = preflight_secret_name_gate(None, false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("missing required argument <NAME>"));
    }

    #[test]
    fn preflight_secret_name_allows_missing_name_when_prompting_allowed() {
        assert!(preflight_secret_name_gate(None, true).is_ok());
    }

    #[test]
    fn view_limit_choice_maps_presets_correctly() {
        assert_eq!(ViewLimitChoice::Single.views(), Some(1));
        assert_eq!(ViewLimitChoice::Two.views(), Some(2));
        assert_eq!(ViewLimitChoice::Three.views(), Some(3));
        assert_eq!(ViewLimitChoice::Five.views(), Some(5));
        assert_eq!(ViewLimitChoice::Ten.views(), Some(10));
        assert_eq!(ViewLimitChoice::Custom.views(), None);

        assert_eq!(
            ViewLimitChoice::Single.to_string(),
            "1 view (burns after read, default)"
        );
        assert_eq!(ViewLimitChoice::Two.to_string(), "2 views");
        assert_eq!(ViewLimitChoice::Custom.to_string(), "Custom view limit...");
    }

    #[test]
    fn lifetime_preset_maps_presets_correctly() {
        assert_eq!(LifetimePreset::NoExpiry.ttl_seconds(), Some(None));
        assert_eq!(LifetimePreset::OneHour.ttl_seconds(), Some(Some(3600)));
        assert_eq!(LifetimePreset::OneDay.ttl_seconds(), Some(Some(86400)));
        assert_eq!(LifetimePreset::SevenDays.ttl_seconds(), Some(Some(604800)));
        assert_eq!(
            LifetimePreset::ThirtyDays.ttl_seconds(),
            Some(Some(MAX_TTL_SECONDS))
        );
        assert_eq!(LifetimePreset::Custom.ttl_seconds(), None);

        assert_eq!(LifetimePreset::NoExpiry.to_string(), "No expiry (default)");
        assert_eq!(LifetimePreset::OneHour.to_string(), "1 hour (3600 seconds)");
        assert_eq!(
            LifetimePreset::Custom.to_string(),
            "Custom lifetime in seconds..."
        );
    }

    #[test]
    fn validate_view_limit_checks_bounds() {
        assert!(validate_view_limit(1).is_ok());
        assert!(validate_view_limit(10).is_ok());
        assert!(validate_view_limit(100).is_ok());

        assert!(validate_view_limit(0).is_err());
        assert!(validate_view_limit(-1).is_err());
        assert!(validate_view_limit(101).is_err());
    }

    #[test]
    fn validate_lifetime_seconds_checks_bounds() {
        assert!(validate_lifetime_seconds(1).is_ok());
        assert!(validate_lifetime_seconds(3600).is_ok());
        assert!(validate_lifetime_seconds(MAX_TTL_SECONDS).is_ok());

        assert!(validate_lifetime_seconds(0).is_err());
        assert!(validate_lifetime_seconds(-1).is_err());
        assert!(validate_lifetime_seconds(MAX_TTL_SECONDS + 1).is_err());
    }
}
