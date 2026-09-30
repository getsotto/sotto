//! TUI application state management for the interactive Sotto dashboard.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::commands::{App, SecretHistoryItem};
use crate::config::Config;
use crate::error::Result;
use crate::store::Store;
use crate::theme::Theme;
use crate::tui::theme::TuiStyles;
use crate::vault::SecretItem;

/// Editing mode for the secret modal dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretModalMode {
    New,
    Edit,
}

/// Active focus field inside the secret modal dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretModalField {
    Name,
    Value,
}

/// Generate a cryptographically secure random secret string using the core CSPRNG.
pub fn generate_random_secret(length: usize) -> String {
    const CHARSET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*(-_=+)";
    let mut buf = vec![0u8; length];
    for chunk in buf.chunks_mut(32) {
        let raw = sotto_core::random::bytes::<32>();
        let to_copy = chunk.len().min(32);
        chunk[..to_copy].copy_from_slice(&raw[..to_copy]);
    }
    buf.iter()
        .map(|&byte| CHARSET[(byte as usize) % CHARSET.len()] as char)
        .collect()
}

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
    pub show_secret_modal: bool,
    pub secret_modal_mode: SecretModalMode,
    pub secret_modal_name: String,
    pub secret_modal_value: Zeroizing<String>,
    pub secret_modal_field: SecretModalField,
    pub secret_modal_masked: bool,
    pub show_delete_modal: bool,
    pub delete_modal_secret_name: String,
    pub show_history_modal: bool,
    pub history_modal_secret_name: String,
    pub history_modal_items: Vec<SecretHistoryItem>,
    pub history_modal_selected_index: usize,
    pub history_modal_revealed: bool,
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
            show_secret_modal: false,
            secret_modal_mode: SecretModalMode::New,
            secret_modal_name: String::new(),
            secret_modal_value: Zeroizing::new(String::new()),
            secret_modal_field: SecretModalField::Name,
            secret_modal_masked: true,
            show_delete_modal: false,
            delete_modal_secret_name: String::new(),
            show_history_modal: false,
            history_modal_secret_name: String::new(),
            history_modal_items: Vec::new(),
            history_modal_selected_index: 0,
            history_modal_revealed: false,
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
        self.show_help = false;
        self.show_secret_modal = false;
        self.show_delete_modal = false;
        self.show_history_modal = false;
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

    /// Open the dialogue to create a new secret in the active environment.
    pub fn open_new_secret_modal(&mut self) {
        self.show_secret_modal = true;
        self.secret_modal_mode = SecretModalMode::New;
        self.secret_modal_name.clear();
        self.secret_modal_value = Zeroizing::new(String::new());
        self.secret_modal_field = SecretModalField::Name;
        self.secret_modal_masked = true;
        self.show_help = false;
        self.show_theme_modal = false;
        self.show_delete_modal = false;
        self.show_history_modal = false;
    }

    /// Open the dialogue to edit the selected secret's value.
    pub fn open_edit_secret_modal(&mut self) -> Result<()> {
        let selected_name = self.selected_secret().map(|s| s.name.clone());
        if let Some(secret_name) = selected_name {
            let value = self.app.get(&self.config, &secret_name)?;
            let zeroized = Zeroizing::new(value);
            match std::str::from_utf8(&zeroized) {
                Ok(text) => {
                    self.show_secret_modal = true;
                    self.secret_modal_mode = SecretModalMode::Edit;
                    self.secret_modal_name = secret_name;
                    self.secret_modal_value = Zeroizing::new(text.to_string());
                    self.secret_modal_field = SecretModalField::Value;
                    self.secret_modal_masked = true;
                    self.show_help = false;
                    self.show_theme_modal = false;
                    self.show_delete_modal = false;
                    self.show_history_modal = false;
                }
                Err(_) => {
                    self.set_status("Cannot edit: secret contains non-UTF-8 bytes".into());
                }
            }
        } else {
            self.set_status("No secret selected to edit".into());
        }
        Ok(())
    }

    /// Close the secret creation or edit modal without saving.
    pub fn close_secret_modal(&mut self) {
        self.show_secret_modal = false;
        self.secret_modal_name.clear();
        self.secret_modal_value = Zeroizing::new(String::new());
    }

    /// Switch to the next field in the secret modal dialogue.
    pub fn secret_modal_next_field(&mut self) {
        if self.secret_modal_mode == SecretModalMode::New {
            self.secret_modal_field = match self.secret_modal_field {
                SecretModalField::Name => SecretModalField::Value,
                SecretModalField::Value => SecretModalField::Name,
            };
        }
    }

    /// Switch to the previous field in the secret modal dialogue.
    pub fn secret_modal_prev_field(&mut self) {
        self.secret_modal_next_field();
    }

    /// Insert a character into the currently focused secret modal field.
    pub fn secret_modal_insert_char(&mut self, c: char) {
        match self.secret_modal_field {
            SecretModalField::Name if self.secret_modal_mode == SecretModalMode::New => {
                self.secret_modal_name.push(c);
            }
            SecretModalField::Value => {
                self.secret_modal_value.push(c);
            }
            _ => {}
        }
    }

    /// Delete the preceding character from the currently focused secret modal field.
    pub fn secret_modal_backspace(&mut self) {
        match self.secret_modal_field {
            SecretModalField::Name if self.secret_modal_mode == SecretModalMode::New => {
                self.secret_modal_name.pop();
            }
            SecretModalField::Value => {
                self.secret_modal_value.pop();
            }
            _ => {}
        }
    }

    /// Toggle masking of the secret value field inside the modal dialogue.
    pub fn toggle_secret_modal_mask(&mut self) {
        self.secret_modal_masked = !self.secret_modal_masked;
    }

    /// Populate the secret modal value field with a cryptographically secure random string.
    pub fn generate_secret_modal_value(&mut self) {
        let generated = generate_random_secret(32);
        self.secret_modal_value = Zeroizing::new(generated);
        self.set_status("Generated 32-character random secret".into());
    }

    /// Commit and save the secret modal contents to the active vault.
    pub fn commit_secret_modal(&mut self) -> Result<()> {
        let name = self.secret_modal_name.trim().to_string();
        if name.is_empty() {
            self.set_status("Secret name cannot be empty".into());
            return Ok(());
        }
        if name.contains(|c: char| c.is_whitespace()) {
            self.set_status("Secret name cannot contain whitespace".into());
            return Ok(());
        }

        let is_new = self.secret_modal_mode == SecretModalMode::New;
        self.app
            .set(&self.config, &name, self.secret_modal_value.as_bytes())?;
        self.refresh_secrets()?;

        if let Some(pos) = self.filtered_indices.iter().position(|&idx| {
            self.secrets
                .get(idx)
                .map(|s| s.name == name)
                .unwrap_or(false)
        }) {
            self.selected_filtered_index = pos;
        }

        if is_new {
            self.set_status(format!(
                "Created secret `{name}` in `{}`",
                self.config.environment
            ));
        } else {
            self.set_status(format!(
                "Updated secret `{name}` in `{}`",
                self.config.environment
            ));
        }

        self.close_secret_modal();
        Ok(())
    }

    /// Open the confirmation dialogue to delete the selected secret.
    pub fn open_delete_modal(&mut self) {
        let selected_name = self.selected_secret().map(|s| s.name.clone());
        if let Some(name) = selected_name {
            self.show_delete_modal = true;
            self.delete_modal_secret_name = name;
            self.show_help = false;
            self.show_theme_modal = false;
            self.show_secret_modal = false;
            self.show_history_modal = false;
        } else {
            self.set_status("No secret selected to delete".into());
        }
    }

    /// Close the delete confirmation dialogue without deleting.
    pub fn close_delete_modal(&mut self) {
        self.show_delete_modal = false;
        self.delete_modal_secret_name.clear();
    }

    /// Confirm and execute the deletion of the selected secret.
    pub fn commit_delete_modal(&mut self) -> Result<()> {
        let name = self.delete_modal_secret_name.clone();
        if name.is_empty() {
            self.close_delete_modal();
            return Ok(());
        }

        self.app.remove(&self.config, &name)?;
        self.refresh_secrets()?;
        self.set_status(format!(
            "Deleted secret `{name}` from `{}`",
            self.config.environment
        ));
        self.close_delete_modal();
        Ok(())
    }

    /// Open the version history dialogue for the selected secret.
    pub fn open_history_modal(&mut self) -> Result<()> {
        let selected_name = self.selected_secret().map(|s| s.name.clone());
        if let Some(secret_name) = selected_name {
            match self.app.secret_history(&self.config, &secret_name) {
                Ok(history) => {
                    self.show_history_modal = true;
                    self.history_modal_secret_name = secret_name;
                    self.history_modal_items = history;
                    self.history_modal_selected_index = 0;
                    self.history_modal_revealed = false;
                    self.show_help = false;
                    self.show_theme_modal = false;
                    self.show_secret_modal = false;
                    self.show_delete_modal = false;
                }
                Err(err) => {
                    self.set_status(format!("Failed to load version history: {err}"));
                }
            }
        } else {
            self.set_status("No secret selected to view history".into());
        }
        Ok(())
    }

    /// Close the version history dialogue.
    pub fn close_history_modal(&mut self) {
        self.show_history_modal = false;
        self.history_modal_revealed = false;
        self.history_modal_items.clear();
        self.history_modal_secret_name.clear();
        self.history_modal_selected_index = 0;
    }

    /// Move selection up in the version history list.
    pub fn history_modal_up(&mut self) {
        if self.history_modal_selected_index > 0 {
            self.history_modal_selected_index -= 1;
        }
    }

    /// Move selection down in the version history list.
    pub fn history_modal_down(&mut self) {
        if !self.history_modal_items.is_empty()
            && self.history_modal_selected_index + 1 < self.history_modal_items.len()
        {
            self.history_modal_selected_index += 1;
        }
    }

    /// Jump to the latest version in the version history list.
    pub fn history_modal_home(&mut self) {
        self.history_modal_selected_index = 0;
    }

    /// Jump to the earliest version in the version history list.
    pub fn history_modal_end(&mut self) {
        if !self.history_modal_items.is_empty() {
            self.history_modal_selected_index = self.history_modal_items.len() - 1;
        }
    }

    /// Toggle masking of secret values in the version history dialogue.
    pub fn toggle_history_reveal(&mut self) {
        self.history_modal_revealed = !self.history_modal_revealed;
    }

    /// Copy the currently selected historical version value to the clipboard.
    pub fn copy_history_selected(&mut self) -> Result<()> {
        if let Some(item) = self
            .history_modal_items
            .get(self.history_modal_selected_index)
        {
            if let Some(val) = &item.value {
                match std::str::from_utf8(val) {
                    Ok(text) => match crate::clipboard::copy(text) {
                        Ok(()) => {
                            let name = &self.history_modal_secret_name;
                            let ver = item.version;
                            self.set_status(format!(
                                "Copied `{name}` (v{ver}) to clipboard (clears in 45s)"
                            ));
                        }
                        Err(err) => {
                            self.set_status(format!("Clipboard copy failed: {err}"));
                        }
                    },
                    Err(_) => {
                        self.set_status("Cannot copy: version contains non-UTF-8 bytes".into());
                    }
                }
            } else {
                self.set_status("Cannot copy: version value is unreadable".into());
            }
        }
        Ok(())
    }

    /// Restore the selected historical version as a new current version (rollback).
    pub fn rollback_history_selected(&mut self) -> Result<()> {
        if let Some(item) = self
            .history_modal_items
            .get(self.history_modal_selected_index)
        {
            let target_version = item.version;
            let secret_name = self.history_modal_secret_name.clone();
            match self
                .app
                .rollback(&self.config, &secret_name, target_version)
            {
                Ok(()) => {
                    self.refresh_secrets()?;
                    let new_version = self
                        .secrets
                        .iter()
                        .find(|s| s.name == secret_name)
                        .map(|s| s.version)
                        .unwrap_or(target_version + 1);
                    self.close_history_modal();
                    self.set_status(format!(
                        "Restored `{secret_name}` to v{target_version} (saved as v{new_version})"
                    ));
                }
                Err(err) => {
                    self.set_status(format!("Rollback failed: {err}"));
                }
            }
        }
        Ok(())
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

    #[test]
    fn secret_modal_new_lifecycle_and_validation() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert!(!tui_app.show_secret_modal);

        // Open new secret modal
        tui_app.open_new_secret_modal();
        assert!(tui_app.show_secret_modal);
        assert_eq!(tui_app.secret_modal_mode, SecretModalMode::New);
        assert_eq!(tui_app.secret_modal_field, SecretModalField::Name);
        assert!(tui_app.secret_modal_masked);

        // Reject empty secret name
        tui_app.commit_secret_modal().unwrap();
        assert!(tui_app.show_secret_modal);
        assert_eq!(
            tui_app.status_message.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Secret name cannot be empty")
        );

        // Reject whitespace in secret name
        tui_app.secret_modal_insert_char('A');
        tui_app.secret_modal_insert_char(' ');
        tui_app.secret_modal_insert_char('B');
        tui_app.commit_secret_modal().unwrap();
        assert!(tui_app.show_secret_modal);
        assert_eq!(
            tui_app.status_message.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Secret name cannot contain whitespace")
        );

        // Backspace and fix name
        tui_app.secret_modal_backspace();
        tui_app.secret_modal_backspace();
        tui_app.secret_modal_insert_char('P');
        tui_app.secret_modal_insert_char('I');
        assert_eq!(tui_app.secret_modal_name, "API");

        // Switch to value field and type
        tui_app.secret_modal_next_field();
        assert_eq!(tui_app.secret_modal_field, SecretModalField::Value);
        tui_app.secret_modal_insert_char('k');
        tui_app.secret_modal_insert_char('e');
        tui_app.secret_modal_insert_char('y');
        tui_app.secret_modal_insert_char('1');
        assert_eq!(tui_app.secret_modal_value.as_str(), "key1");

        // Toggle mask
        assert!(tui_app.secret_modal_masked);
        tui_app.toggle_secret_modal_mask();
        assert!(!tui_app.secret_modal_masked);

        // Commit secret creation
        tui_app.commit_secret_modal().unwrap();
        assert!(!tui_app.show_secret_modal);
        assert_eq!(tui_app.secrets.len(), 1);
        assert_eq!(tui_app.secrets[0].name, "API");
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("API")
        );

        // Verify stored value
        let fetched = app.get(&tui_app.config, "API").unwrap();
        assert_eq!(fetched, b"key1");
    }

    #[test]
    fn secret_modal_edit_lifecycle() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "EXISTING_KEY", b"initial-secret").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(
            tui_app.selected_secret().map(|s| s.name.as_str()),
            Some("EXISTING_KEY")
        );

        // Open edit modal
        tui_app.open_edit_secret_modal().unwrap();
        assert!(tui_app.show_secret_modal);
        assert_eq!(tui_app.secret_modal_mode, SecretModalMode::Edit);
        assert_eq!(tui_app.secret_modal_name, "EXISTING_KEY");
        assert_eq!(tui_app.secret_modal_value.as_str(), "initial-secret");
        assert_eq!(tui_app.secret_modal_field, SecretModalField::Value);

        // Edit value
        tui_app.secret_modal_value = Zeroizing::new("updated-secret".into());
        tui_app.commit_secret_modal().unwrap();
        assert!(!tui_app.show_secret_modal);

        // Verify updated value in vault
        let fetched = app.get(&tui_app.config, "EXISTING_KEY").unwrap();
        assert_eq!(fetched, b"updated-secret");
    }

    #[test]
    fn secret_modal_random_generator() {
        let secret1 = generate_random_secret(32);
        let secret2 = generate_random_secret(32);
        assert_eq!(secret1.len(), 32);
        assert_eq!(secret2.len(), 32);
        assert_ne!(secret1, secret2);

        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();

        tui_app.open_new_secret_modal();
        tui_app.generate_secret_modal_value();
        assert_eq!(tui_app.secret_modal_value.len(), 32);
        assert!(tui_app.status_message.is_some());
    }

    #[test]
    fn delete_modal_lifecycle_and_execution() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "TARGET", b"val").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.secrets.len(), 1);

        // Open delete modal
        tui_app.open_delete_modal();
        assert!(tui_app.show_delete_modal);
        assert_eq!(tui_app.delete_modal_secret_name, "TARGET");

        // Cancel delete
        tui_app.close_delete_modal();
        assert!(!tui_app.show_delete_modal);
        assert_eq!(tui_app.secrets.len(), 1);

        // Re-open and commit delete
        tui_app.open_delete_modal();
        tui_app.commit_delete_modal().unwrap();
        assert!(!tui_app.show_delete_modal);
        assert_eq!(tui_app.secrets.len(), 0);
        assert_eq!(
            tui_app.status_message.as_ref().map(|(m, _)| m.as_str()),
            Some("Deleted secret `TARGET` from `dev`")
        );
    }

    #[test]
    fn version_history_modal_lifecycle_and_rollback() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);

        // Populate a secret with 3 versions
        app.set(&config, "HOST_KEY", b"v1-initial").unwrap();
        app.set(&config, "HOST_KEY", b"v2-staging").unwrap();
        app.set(&config, "HOST_KEY", b"v3-production").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.secrets.len(), 1);
        assert_eq!(tui_app.secrets[0].version, 3);

        // Open history modal
        tui_app.open_history_modal().unwrap();
        assert!(tui_app.show_history_modal);
        assert_eq!(tui_app.history_modal_secret_name, "HOST_KEY");
        assert_eq!(tui_app.history_modal_items.len(), 3);
        assert_eq!(tui_app.history_modal_selected_index, 0);
        assert_eq!(tui_app.history_modal_items[0].version, 3);
        assert_eq!(tui_app.history_modal_items[1].version, 2);
        assert_eq!(tui_app.history_modal_items[2].version, 1);

        // Test reveal toggle
        assert!(!tui_app.history_modal_revealed);
        tui_app.toggle_history_reveal();
        assert!(tui_app.history_modal_revealed);

        // Test navigation
        tui_app.history_modal_down();
        assert_eq!(tui_app.history_modal_selected_index, 1); // pointing to v2
        tui_app.history_modal_down();
        assert_eq!(tui_app.history_modal_selected_index, 2); // pointing to v1
        tui_app.history_modal_down();
        assert_eq!(tui_app.history_modal_selected_index, 2); // clamped at bottom
        tui_app.history_modal_up();
        assert_eq!(tui_app.history_modal_selected_index, 1); // back to v2
        tui_app.history_modal_home();
        assert_eq!(tui_app.history_modal_selected_index, 0); // v3
        tui_app.history_modal_end();
        assert_eq!(tui_app.history_modal_selected_index, 2); // v1

        // Move to v2 and roll back
        tui_app.history_modal_selected_index = 1;
        tui_app.rollback_history_selected().unwrap();

        // Verify modal closed, secret restored to v2 value and recorded as v4
        assert!(!tui_app.show_history_modal);
        assert_eq!(tui_app.secrets[0].version, 4);
        let restored_val = app.get(&tui_app.config, "HOST_KEY").unwrap();
        assert_eq!(restored_val, b"v2-staging");
        assert_eq!(
            tui_app.status_message.as_ref().map(|(m, _)| m.as_str()),
            Some("Restored `HOST_KEY` to v2 (saved as v4)")
        );
    }
}
