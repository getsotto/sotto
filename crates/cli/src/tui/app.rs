//! TUI application state management for the interactive Sotto dashboard.

use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::commands::App;
use crate::config::Config;
use crate::error::Result;
use crate::store::Store;
use crate::theme::Theme;
use crate::tui::theme::TuiStyles;
use crate::vault::SecretItem;

/// Application state for the interactive split-pane dashboard.
pub struct TuiApp<'a> {
    pub app: &'a App<'a>,
    pub store: &'a Store,
    pub config: Config,
    pub styles: TuiStyles,
    pub environments: Vec<String>,
    pub active_env_index: usize,
    pub secrets: Vec<SecretItem>,
    pub filtered_indices: Vec<usize>,
    pub selected_filtered_index: usize,
    pub search_query: String,
    pub search_mode: bool,
    pub revealed: bool,
    pub decrypted_cache: Option<Zeroizing<Vec<u8>>>,
    pub show_help: bool,
    pub status_message: Option<(String, Instant)>,
    pub running: bool,
}

impl<'a> TuiApp<'a> {
    /// Initialise a new TUI dashboard state from the project configuration and active theme.
    pub fn new(
        app: &'a App<'a>,
        store: &'a Store,
        config: Config,
        theme: &'a Theme,
    ) -> Result<Self> {
        let styles = TuiStyles::from_theme(theme);
        let mut environments = store.list_environments(&config.project_id)?;
        if environments.is_empty() {
            environments.push(config.environment.clone());
        }
        environments.sort();

        let active_env_index = environments
            .iter()
            .position(|e| e == &config.environment)
            .unwrap_or(0);

        let mut app_state = Self {
            app,
            store,
            config,
            styles,
            environments,
            active_env_index,
            secrets: Vec::new(),
            filtered_indices: Vec::new(),
            selected_filtered_index: 0,
            search_query: String::new(),
            search_mode: false,
            revealed: false,
            decrypted_cache: None,
            show_help: false,
            status_message: None,
            running: true,
        };

        app_state.refresh_secrets()?;
        Ok(app_state)
    }

    /// Reload secrets from the active environment.
    pub fn refresh_secrets(&mut self) -> Result<()> {
        let mut items = self.app.list_items(&self.config)?;
        items.sort_by(|a, b| a.name.cmp(&b.name));
        self.secrets = items;
        self.apply_filter();
        self.reset_secret_view();
        Ok(())
    }

    /// Filter the secret list using the current search query.
    pub fn apply_filter(&mut self) {
        if self.search_query.trim().is_empty() {
            self.filtered_indices = (0..self.secrets.len()).collect();
        } else {
            let query = self.search_query.to_lowercase();
            self.filtered_indices = self
                .secrets
                .iter()
                .enumerate()
                .filter(|(_, item)| item.name.to_lowercase().contains(&query))
                .map(|(idx, _)| idx)
                .collect();
        }

        if self.filtered_indices.is_empty() {
            self.selected_filtered_index = 0;
        } else if self.selected_filtered_index >= self.filtered_indices.len() {
            self.selected_filtered_index = self.filtered_indices.len() - 1;
        }
    }

    /// Reset any revealed or cached secret cleartext.
    pub fn reset_secret_view(&mut self) {
        self.revealed = false;
        self.decrypted_cache = None;
    }

    /// Get the currently highlighted secret item, if one exists.
    pub fn selected_secret(&self) -> Option<&SecretItem> {
        self.filtered_indices
            .get(self.selected_filtered_index)
            .and_then(|&orig_idx| self.secrets.get(orig_idx))
    }

    /// Move the cursor selection up.
    pub fn move_selection_up(&mut self) {
        if self.selected_filtered_index > 0 {
            self.selected_filtered_index -= 1;
            self.reset_secret_view();
        }
    }

    /// Move the cursor selection down.
    pub fn move_selection_down(&mut self) {
        if !self.filtered_indices.is_empty()
            && self.selected_filtered_index + 1 < self.filtered_indices.len()
        {
            self.selected_filtered_index += 1;
            self.reset_secret_view();
        }
    }

    /// Toggle reveal of the selected secret value.
    pub fn toggle_reveal(&mut self) -> Result<()> {
        if self.revealed {
            self.reset_secret_view();
        } else if let Some(item) = self.selected_secret() {
            let value = self.app.get(&self.config, &item.name)?;
            self.decrypted_cache = Some(Zeroizing::new(value));
            self.revealed = true;
        }
        Ok(())
    }

    /// Copy the selected secret to the system clipboard with an auto-clear timer.
    pub fn copy_selected(&mut self) -> Result<()> {
        if let Some(item) = self.selected_secret() {
            let secret_name = item.name.clone();
            let value = self.app.get(&self.config, &secret_name)?;
            let zeroized = Zeroizing::new(value);
            match std::str::from_utf8(&zeroized) {
                Ok(text) => match crate::clipboard::copy(text) {
                    Ok(()) => {
                        self.set_status(format!(
                            "Copied `{secret_name}` to clipboard (clears in 45s)"
                        ));
                    }
                    Err(err) => {
                        self.set_status(format!("Clipboard copy failed: {err}"));
                    }
                },
                Err(_) => {
                    self.set_status("Cannot copy: secret contains non-UTF-8 bytes".into());
                }
            }
        }
        Ok(())
    }

    /// Switch to the next available environment in the project.
    pub fn cycle_environment(&mut self) -> Result<()> {
        if self.environments.len() <= 1 {
            self.set_status("Only one environment configured".into());
            return Ok(());
        }

        self.active_env_index = (self.active_env_index + 1) % self.environments.len();
        self.config.environment = self.environments[self.active_env_index].clone();
        self.search_query.clear();
        self.search_mode = false;
        self.refresh_secrets()?;
        self.set_status(format!(
            "Switched to environment `{}`",
            self.config.environment
        ));
        Ok(())
    }

    /// Toggle the help modal cheatsheet.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Record a transient status notification message.
    pub fn set_status(&mut self, message: String) {
        self.status_message = Some((message, Instant::now()));
    }

    /// Retrieve the current status notification if it hasn't expired.
    pub fn active_status(&self) -> Option<&str> {
        if let Some((msg, time)) = &self.status_message {
            if time.elapsed() < Duration::from_secs(5) {
                return Some(msg.as_str());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use crate::session;
    use crate::store::Store;
    use crate::vault::Vault;
    use std::time::Duration;

    fn unlocked() -> (Store, MemoryKeychain, Config) {
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(&store, &keychain, b"pw", Duration::from_secs(3600)).unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        (store, keychain, config)
    }

    #[test]
    fn app_navigation_and_reveal() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "ALPHA", b"secret-alpha").unwrap();
        app.set(&config, "BETA", b"secret-beta").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.secrets.len(), 2);
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("ALPHA")
        );
        assert!(!tui_app.revealed);

        // Move down to BETA
        tui_app.move_selection_down();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("BETA")
        );

        // Reveal BETA
        tui_app.toggle_reveal().unwrap();
        assert!(tui_app.revealed);
        assert_eq!(
            tui_app.decrypted_cache.as_deref().map(|z| z.as_slice()),
            Some(b"secret-beta".as_slice())
        );

        // Moving selection resets reveal
        tui_app.move_selection_up();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("ALPHA")
        );
        assert!(!tui_app.revealed);
        assert!(tui_app.decrypted_cache.is_none());

        // Toggle help overlay
        assert!(!tui_app.show_help);
        tui_app.toggle_help();
        assert!(tui_app.show_help);
        tui_app.toggle_help();
        assert!(!tui_app.show_help);
    }

    #[test]
    fn app_filtering() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "DATABASE_URL", b"postgres://localhost")
            .unwrap();
        app.set(&config, "API_KEY", b"secret-key").unwrap();
        app.set(&config, "DATA_DIR", b"/var/data").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.filtered_indices.len(), 3);

        tui_app.search_query = "data".into();
        tui_app.apply_filter();
        assert_eq!(tui_app.filtered_indices.len(), 2);
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("DATABASE_URL")
        );

        tui_app.move_selection_down();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("DATA_DIR")
        );

        tui_app.search_query = "none".into();
        tui_app.apply_filter();
        assert!(tui_app.filtered_indices.is_empty());
        assert!(tui_app.selected_secret().is_none());
    }

    #[test]
    fn app_cycle_environment() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.config.environment, "dev");

        // Cycle environments: dev -> prod -> staging -> dev
        tui_app.cycle_environment().unwrap();
        assert_eq!(tui_app.config.environment, "prod");
        tui_app.cycle_environment().unwrap();
        assert_eq!(tui_app.config.environment, "staging");
        tui_app.cycle_environment().unwrap();
        assert_eq!(tui_app.config.environment, "dev");
    }
}
