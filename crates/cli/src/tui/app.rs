//! TUI application state management for the interactive Sotto dashboard.

use std::path::PathBuf;
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
    pub config_path: Option<PathBuf>,
    pub styles: TuiStyles,
    pub current_theme: Theme,
    pub original_theme: Theme,
    pub available_themes: Vec<Theme>,
    pub selected_theme_index: usize,
    pub show_theme_modal: bool,
    pub environments: Vec<String>,
    pub active_env_index: usize,
    pub secrets: Vec<SecretItem>,
    pub filtered_indices: Vec<usize>,
    pub selected_filtered_index: usize,
    pub search_query: String,
    pub search_mode: bool,
    pub revealed: bool,
    pub decrypted_cache: Option<Zeroizing<Vec<u8>>>,
    pub reveal_animation_start: Option<Instant>,
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
            config_path: crate::paths::config_path().ok(),
            styles,
            current_theme: theme.clone(),
            original_theme: theme.clone(),
            available_themes: Vec::new(),
            selected_theme_index: 0,
            show_theme_modal: false,
            environments,
            active_env_index,
            secrets: Vec::new(),
            filtered_indices: Vec::new(),
            selected_filtered_index: 0,
            search_query: String::new(),
            search_mode: false,
            revealed: false,
            decrypted_cache: None,
            reveal_animation_start: None,
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

        self.reset_secret_view();
    }

    /// Reset any revealed or cached secret cleartext and ongoing reveal animations.
    pub fn reset_secret_view(&mut self) {
        self.revealed = false;
        self.decrypted_cache = None;
        self.reveal_animation_start = None;
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

    /// Move cursor selection to the beginning of the list.
    pub fn move_selection_home(&mut self) {
        if !self.filtered_indices.is_empty() && self.selected_filtered_index > 0 {
            self.selected_filtered_index = 0;
            self.reset_secret_view();
        }
    }

    /// Move cursor selection to the end of the list.
    pub fn move_selection_end(&mut self) {
        if !self.filtered_indices.is_empty() {
            let last_idx = self.filtered_indices.len() - 1;
            if self.selected_filtered_index != last_idx {
                self.selected_filtered_index = last_idx;
                self.reset_secret_view();
            }
        }
    }

    /// Move cursor selection up by a page step.
    pub fn page_up(&mut self, step: usize) {
        if self.selected_filtered_index > 0 {
            self.selected_filtered_index = self.selected_filtered_index.saturating_sub(step);
            self.reset_secret_view();
        }
    }

    /// Move cursor selection down by a page step.
    pub fn page_down(&mut self, step: usize) {
        if !self.filtered_indices.is_empty() {
            let last_idx = self.filtered_indices.len() - 1;
            if self.selected_filtered_index < last_idx {
                self.selected_filtered_index = (self.selected_filtered_index + step).min(last_idx);
                self.reset_secret_view();
            }
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
            self.reveal_animation_start = Some(Instant::now());
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

    /// Open the theme switcher modal, discovering available themes and capturing the original theme.
    pub fn open_theme_modal(&mut self) {
        let themes_dir = crate::paths::themes_path().ok();
        let mut themes = crate::theme::available_themes(themes_dir.as_deref());
        if themes.is_empty() {
            themes = Theme::presets();
        }

        let selected_idx = themes
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(&self.current_theme.name))
            .unwrap_or(0);

        self.original_theme = self.current_theme.clone();
        self.available_themes = themes;
        self.selected_theme_index = selected_idx;
        self.show_theme_modal = true;
        self.preview_selected_theme();
    }

    /// Update the active styles and current theme to preview the currently selected theme item.
    pub fn preview_selected_theme(&mut self) {
        if let Some(selected) = self.available_themes.get(self.selected_theme_index) {
            let active = self.original_theme.active;
            let preview = selected.clone().with_active(active);
            self.styles = TuiStyles::from_theme(&preview);
            self.current_theme = preview;
        }
    }

    /// Select the next theme in the modal list with real-time preview.
    pub fn next_theme(&mut self) {
        if !self.available_themes.is_empty() {
            self.selected_theme_index =
                (self.selected_theme_index + 1) % self.available_themes.len();
            self.preview_selected_theme();
        }
    }

    /// Select the previous theme in the modal list with real-time preview.
    pub fn previous_theme(&mut self) {
        if !self.available_themes.is_empty() {
            if self.selected_theme_index == 0 {
                self.selected_theme_index = self.available_themes.len() - 1;
            } else {
                self.selected_theme_index -= 1;
            }
            self.preview_selected_theme();
        }
    }

    /// Jump to the first theme in the modal list.
    pub fn theme_home(&mut self) {
        if !self.available_themes.is_empty() {
            self.selected_theme_index = 0;
            self.preview_selected_theme();
        }
    }

    /// Jump to the last theme in the modal list.
    pub fn theme_end(&mut self) {
        if !self.available_themes.is_empty() {
            self.selected_theme_index = self.available_themes.len() - 1;
            self.preview_selected_theme();
        }
    }

    /// Commit the selected theme and persist the choice into the user configuration.
    pub fn commit_theme(&mut self) -> Result<()> {
        if let Some(selected) = self.available_themes.get(self.selected_theme_index) {
            let theme_name = selected.name.clone();
            let target_path = self
                .config_path
                .clone()
                .or_else(|| crate::paths::config_path().ok());

            match target_path {
                Some(config_path) => {
                    if let Some(parent) = config_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(err) = crate::theme::save_theme_preference(&theme_name, &config_path)
                    {
                        self.set_status(format!("Failed to persist theme preference: {err}"));
                    } else {
                        self.set_status(format!("Theme set to `{theme_name}`"));
                    }
                }
                None => {
                    self.set_status("Failed to locate configuration file".into());
                }
            }
        }
        self.show_theme_modal = false;
        Ok(())
    }

    /// Revert any live preview back to the original theme and close the modal.
    pub fn revert_theme(&mut self) {
        self.current_theme = self.original_theme.clone();
        self.styles = TuiStyles::from_theme(&self.current_theme);
        self.show_theme_modal = false;
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

    #[test]
    fn app_filter_resets_revealed_cache() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "ALPHA", b"alpha-val").unwrap();
        app.set(&config, "BETA", b"beta-val").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        // Move to BETA and reveal it
        tui_app.move_selection_down();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("BETA")
        );
        tui_app.toggle_reveal().unwrap();
        assert!(tui_app.revealed);
        assert!(tui_app.decrypted_cache.is_some());

        // Narrow filter to ALPHA
        tui_app.search_query = "ALPHA".into();
        tui_app.apply_filter();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("ALPHA")
        );
        // Reveal state and cache must be reset so BETA's plaintext is not displayed for ALPHA
        assert!(!tui_app.revealed);
        assert!(tui_app.decrypted_cache.is_none());
    }

    #[test]
    fn app_extended_navigation() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        for i in 0..25 {
            app.set(&config, &format!("KEY_{i:02}"), b"val").unwrap();
        }

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 0);

        // End moves to last item
        tui_app.move_selection_end();
        assert_eq!(tui_app.selected_filtered_index, 24);

        // Home moves to first item
        tui_app.move_selection_home();
        assert_eq!(tui_app.selected_filtered_index, 0);

        // Page down moves by step
        tui_app.page_down(10);
        assert_eq!(tui_app.selected_filtered_index, 10);
        tui_app.page_down(10);
        assert_eq!(tui_app.selected_filtered_index, 20);
        // Page down clamps to last index
        tui_app.page_down(10);
        assert_eq!(tui_app.selected_filtered_index, 24);

        // Page up moves up and clamps to 0
        tui_app.page_up(10);
        assert_eq!(tui_app.selected_filtered_index, 14);
        tui_app.page_up(20);
        assert_eq!(tui_app.selected_filtered_index, 0);
    }

    #[test]
    fn all_movement_paths_reset_revealed_cache() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        for i in 0..10 {
            app.set(
                &config,
                &format!("KEY_{i:02}"),
                format!("secret-value-{i}").as_bytes(),
            )
            .unwrap();
        }

        struct MovementCase {
            name: &'static str,
            start_idx: usize,
            action: fn(&mut TuiApp),
        }

        let movements = [
            MovementCase {
                name: "move_selection_up",
                start_idx: 5,
                action: |a| a.move_selection_up(),
            },
            MovementCase {
                name: "move_selection_down",
                start_idx: 5,
                action: |a| a.move_selection_down(),
            },
            MovementCase {
                name: "move_selection_home",
                start_idx: 5,
                action: |a| a.move_selection_home(),
            },
            MovementCase {
                name: "move_selection_end",
                start_idx: 5,
                action: |a| a.move_selection_end(),
            },
            MovementCase {
                name: "page_up",
                start_idx: 5,
                action: |a| a.page_up(2),
            },
            MovementCase {
                name: "page_down",
                start_idx: 5,
                action: |a| a.page_down(2),
            },
        ];

        for case in movements {
            let mut tui_app = TuiApp::new(&app, &store, config.clone(), &theme).unwrap();
            tui_app.selected_filtered_index = case.start_idx;
            tui_app.toggle_reveal().unwrap();
            assert!(
                tui_app.revealed,
                "secret must be revealed before move for {}",
                case.name
            );
            assert!(
                tui_app.decrypted_cache.is_some(),
                "cache must be populated before move for {}",
                case.name
            );

            // Execute movement
            (case.action)(&mut tui_app);

            assert!(
                !tui_app.revealed,
                "{} must reset revealed state to false",
                case.name
            );
            assert!(
                tui_app.decrypted_cache.is_none(),
                "{} must clear decrypted cleartext cache",
                case.name
            );
        }
    }

    #[test]
    fn theme_modal_lifecycle_and_live_preview() {
        let (store, keychain, config) = unlocked();
        let initial_theme = Theme::nord();
        let app = App::new(&store, &keychain);
        let mut tui_app = TuiApp::new(&app, &store, config, &initial_theme).unwrap();

        assert!(!tui_app.show_theme_modal);
        assert_eq!(tui_app.current_theme.name, "nord");

        // Open modal
        tui_app.open_theme_modal();
        assert!(tui_app.show_theme_modal);
        assert!(!tui_app.available_themes.is_empty());
        assert_eq!(tui_app.original_theme.name, "nord");

        // Cycle through themes and observe live style updates
        let initial_accent = tui_app.styles.accent;
        tui_app.next_theme();
        let next_theme_name = tui_app.current_theme.name.clone();
        assert_ne!(next_theme_name, "nord");
        let next_accent = tui_app.styles.accent;
        assert_ne!(initial_accent, next_accent);

        // Test theme home and end
        tui_app.theme_end();
        assert_eq!(
            tui_app.selected_theme_index,
            tui_app.available_themes.len() - 1
        );
        tui_app.theme_home();
        assert_eq!(tui_app.selected_theme_index, 0);

        // Reverting restores original theme and styles
        tui_app.next_theme();
        assert_ne!(tui_app.current_theme.name, "nord");
        tui_app.revert_theme();
        assert!(!tui_app.show_theme_modal);
        assert_eq!(tui_app.current_theme.name, "nord");
        assert_eq!(tui_app.styles.accent, initial_accent);

        // Committing theme keeps preview and closes modal with isolated config path
        let temp_dir = tempfile::tempdir().unwrap();
        let isolated_config = temp_dir.path().join("config.toml");
        tui_app.config_path = Some(isolated_config.clone());

        tui_app.open_theme_modal();
        tui_app.next_theme();
        let committed_name = tui_app.current_theme.name.clone();
        tui_app.commit_theme().unwrap();
        assert!(!tui_app.show_theme_modal);
        assert_eq!(tui_app.current_theme.name, committed_name);

        // Verify written config matches committed choice
        let loaded = crate::remote::config::GlobalConfig::load_from(&isolated_config)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.theme.as_deref(), Some(committed_name.as_str()));
    }

    #[test]
    fn reveal_animation_trigger_and_teardown() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "SECRET_KEY", b"super-secret-value")
            .unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert!(tui_app.reveal_animation_start.is_none());

        // Reveal arms the animation start timestamp
        tui_app.toggle_reveal().unwrap();
        assert!(tui_app.revealed);
        assert!(tui_app.reveal_animation_start.is_some());

        // Reset clears reveal and animation start timestamp
        tui_app.reset_secret_view();
        assert!(!tui_app.revealed);
        assert!(tui_app.reveal_animation_start.is_none());
    }
}
