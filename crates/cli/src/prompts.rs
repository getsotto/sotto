//! Interactive terminal prompt helpers for CLI subcommands.
//!
//! When required arguments are omitted in an interactive terminal (stdin, stdout, and stderr
//! are all TTYs and no override disables styling/prompts), these functions provide fuzzy/selectable
//! menus using `inquire`.
//!
//! If the session is non-interactive (e.g. piped input/output, CI, or --plain/NO_COLOR),
//! missing arguments result in an immediate input error with standard exit code 1 or 2.

use std::io::{self, IsTerminal};

use inquire::{Confirm, Select};

use crate::commands::App;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::theme::Theme;

/// Check whether interactive prompting is permissible.
///
/// Prompts require an interactive terminal across stdin, stdout, and stderr,
/// and that CI is not active.
pub fn can_prompt() -> bool {
    allowed(
        std::env::args().any(|arg| arg == "--plain"),
        std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
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
    if name.is_none() && !can_prompt() {
        return Err(Error::Input(
            "missing required argument <NAME>; provide a secret name or run in an interactive terminal".into(),
        ));
    }
    Ok(())
}

/// Prompt the user to select a secret name from the active environment.
pub fn select_secret_key(app: &App, config: &Config, theme: &Theme) -> Result<String> {
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

    Select::new(&message, names)
        .prompt()
        .map_err(|e| Error::Input(format!("selection cancelled or failed: {e}")))
}

/// Prompt the user to select an environment from the project's environments.
pub fn select_environment(store: &Store, project_id: &str, theme: &Theme) -> Result<String> {
    if !can_prompt() {
        return Err(Error::Input(
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

    Select::new(&message, environments)
        .prompt()
        .map_err(|e| Error::Input(format!("selection cancelled or failed: {e}")))
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

    Confirm::new(&prompt)
        .with_default(false)
        .prompt()
        .map_err(|e| Error::Input(format!("prompt cancelled: {e}")))
}

/// Pure testable gate for whether interactive prompts are allowed.
pub fn allowed(
    plain: bool,
    no_color: bool,
    ci: bool,
    dumb_terminal: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> bool {
    !plain && !no_color && !ci && !dumb_terminal && stdin_tty && stdout_tty && stderr_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_requires_interactive_terminal_on_all_streams() {
        assert!(allowed(false, false, false, false, true, true, true));
        assert!(!allowed(true, false, false, false, true, true, true));
        assert!(!allowed(false, true, false, false, true, true, true));
        assert!(!allowed(false, false, true, false, true, true, true));
        assert!(!allowed(false, false, false, true, true, true, true));
        assert!(!allowed(false, false, false, false, false, true, true));
        assert!(!allowed(false, false, false, false, true, false, true));
        assert!(!allowed(false, false, false, false, true, true, false));
    }

    #[test]
    fn preflight_secret_name_accepts_present_name() {
        assert!(preflight_secret_name(Some("DATABASE_URL")).is_ok());
    }

    #[test]
    fn preflight_secret_name_rejects_missing_name_in_non_interactive_env() {
        if !can_prompt() {
            assert!(matches!(
                preflight_secret_name(None),
                Err(Error::Input(msg)) if msg.contains("missing required argument <NAME>")
            ));
        }
    }
}
