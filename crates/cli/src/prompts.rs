//! Interactive terminal prompt helpers for CLI subcommands.
//!
//! When required arguments are omitted in an interactive terminal (stdin, stdout, and stderr
//! are all TTYs, `TERM` is not dumb, and CI is not active), these functions provide
//! selectable menus using `inquire`.
//!
//! If the session is non-interactive (e.g. piped input/output or CI), missing arguments
//! result in an immediate input error with standard exit code 2 (matching clap).

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
        Err(inquire::InquireError::OperationCanceled) => Ok(None),
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
        Err(inquire::InquireError::OperationCanceled) => Ok(None),
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
        Err(inquire::InquireError::OperationCanceled) => Ok(false),
        Err(e) => Err(Error::Input(format!("prompt failed: {e}"))),
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
}
