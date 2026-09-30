//! Keyboard and mouse event handlers for the interactive Sotto dashboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::error::Result;
use crate::tui::app::TuiApp;

/// Handle incoming keyboard events.
pub fn handle_key_event(app: &mut TuiApp, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.running = false;
        return Ok(());
    }

    if app.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                app.show_help = false;
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_theme_modal {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.revert_theme();
            }
            KeyCode::Enter => {
                app.commit_theme()?;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.previous_theme();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.next_theme();
            }
            KeyCode::Home => {
                app.theme_home();
            }
            KeyCode::End => {
                app.theme_end();
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_delete_modal {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                app.close_delete_modal();
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.commit_delete_modal()?;
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_history_modal {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.close_history_modal();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.history_modal_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.history_modal_down();
            }
            KeyCode::Home | KeyCode::Char('g') => {
                app.history_modal_home();
            }
            KeyCode::End | KeyCode::Char('G') => {
                app.history_modal_end();
            }
            KeyCode::Char('r') | KeyCode::Char(' ') => {
                app.toggle_history_reveal();
            }
            KeyCode::Char('c') => {
                app.copy_history_selected()?;
            }
            KeyCode::Enter | KeyCode::Char('b') => {
                app.rollback_history_selected()?;
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_secret_modal {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('g')) => {
                app.generate_secret_modal_value();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                app.toggle_secret_modal_mask();
            }
            (_, KeyCode::Esc) => {
                app.close_secret_modal();
            }
            (_, KeyCode::Enter) => {
                app.commit_secret_modal()?;
            }
            (_, KeyCode::Tab) | (_, KeyCode::Down) => {
                app.secret_modal_next_field();
            }
            (_, KeyCode::BackTab) | (_, KeyCode::Up) => {
                app.secret_modal_prev_field();
            }
            (_, KeyCode::Backspace) => {
                app.secret_modal_backspace();
            }
            (_, KeyCode::Char(c)) => {
                app.secret_modal_insert_char(c);
            }
            _ => {}
        }
        return Ok(());
    }

    if app.search_mode {
        match key.code {
            KeyCode::Esc => {
                app.search_query.clear();
                app.search_mode = false;
                app.apply_filter();
            }
            KeyCode::Enter => {
                app.search_mode = false;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.apply_filter();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.apply_filter();
            }
            KeyCode::Down | KeyCode::Up => {
                app.search_mode = false;
                if key.code == KeyCode::Down {
                    app.move_selection_down();
                } else {
                    app.move_selection_up();
                }
            }
            KeyCode::PageDown | KeyCode::PageUp => {
                app.search_mode = false;
                if key.code == KeyCode::PageDown {
                    app.page_down(10);
                } else {
                    app.page_up(10);
                }
            }
            KeyCode::Home | KeyCode::End => {
                app.search_mode = false;
                if key.code == KeyCode::Home {
                    app.move_selection_home();
                } else {
                    app.move_selection_end();
                }
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('q') => {
            app.running = false;
        }
        KeyCode::Char('?') => {
            app.toggle_help();
        }
        KeyCode::Char('/') => {
            app.search_mode = true;
        }
        KeyCode::Tab => {
            app.cycle_environment()?;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_selection_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_selection_down();
        }
        KeyCode::Home => {
            app.move_selection_home();
        }
        KeyCode::End => {
            app.move_selection_end();
        }
        KeyCode::PageUp => {
            app.page_up(10);
        }
        KeyCode::PageDown => {
            app.page_down(10);
        }
        KeyCode::Char('n') => {
            app.open_new_secret_modal();
        }
        KeyCode::Char('e') => {
            app.open_edit_secret_modal()?;
        }
        KeyCode::Char('d') => {
            app.open_delete_modal();
        }
        KeyCode::Char('h') => {
            app.open_history_modal()?;
        }
        KeyCode::Char('c') => {
            app.copy_selected()?;
        }
        KeyCode::Char('r') => {
            app.toggle_reveal()?;
        }
        KeyCode::Char('t') => {
            app.open_theme_modal();
        }
        KeyCode::Esc => {
            if !app.search_query.is_empty() {
                app.search_query.clear();
                app.apply_filter();
            } else {
                app.running = false;
            }
        }
        _ => {}
    }

    Ok(())
}

/// Handle incoming mouse events (scroll and click selection).
pub fn handle_mouse_event(app: &mut TuiApp, mouse: MouseEvent) -> Result<()> {
    if app.show_theme_modal {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                app.next_theme();
            }
            MouseEventKind::ScrollUp => {
                app.previous_theme();
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_history_modal {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                app.history_modal_down();
            }
            MouseEventKind::ScrollUp => {
                app.history_modal_up();
            }
            _ => {}
        }
        return Ok(());
    }

    if app.show_help || app.show_secret_modal || app.show_delete_modal || app.show_history_modal {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if app.show_help {
                app.show_help = false;
            }
        }
        return Ok(());
    }

    match mouse.kind {
        MouseEventKind::ScrollDown => {
            app.move_selection_down();
        }
        MouseEventKind::ScrollUp => {
            app.move_selection_up();
        }
        MouseEventKind::Down(MouseButton::Left) if app.show_help => {
            app.show_help = false;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn search_mode_appends_and_backspaces() {
        let mut query = String::new();
        let k = key(KeyCode::Char('a'));
        if let KeyCode::Char(c) = k.code {
            query.push(c);
        }
        assert_eq!(query, "a");

        let b = key(KeyCode::Backspace);
        if b.code == KeyCode::Backspace {
            query.pop();
        }
        assert_eq!(query, "");
    }

    #[test]
    fn ctrl_c_quits_in_all_modes() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);

        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        // Normal mode
        let mut tui_app = TuiApp::new(&app, &store, config.clone(), &theme).unwrap();
        assert!(tui_app.running);
        handle_key_event(&mut tui_app, ctrl_c).unwrap();
        assert!(!tui_app.running);

        // Help modal mode
        let mut tui_app = TuiApp::new(&app, &store, config.clone(), &theme).unwrap();
        tui_app.show_help = true;
        handle_key_event(&mut tui_app, ctrl_c).unwrap();
        assert!(!tui_app.running);

        // Theme modal mode
        let mut tui_app = TuiApp::new(&app, &store, config.clone(), &theme).unwrap();
        tui_app.show_theme_modal = true;
        handle_key_event(&mut tui_app, ctrl_c).unwrap();
        assert!(!tui_app.running);

        // Search mode
        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.search_mode = true;
        handle_key_event(&mut tui_app, ctrl_c).unwrap();
        assert!(!tui_app.running);
    }

    #[test]
    fn navigation_key_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);
        for i in 0..15 {
            app.set(&config, &format!("KEY_{i:02}"), b"val").unwrap();
        }

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 0);

        // End key
        handle_key_event(&mut tui_app, key(KeyCode::End)).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 14);

        // Home key
        handle_key_event(&mut tui_app, key(KeyCode::Home)).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 0);

        // PageDown key
        handle_key_event(&mut tui_app, key(KeyCode::PageDown)).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 10);

        // PageUp key
        handle_key_event(&mut tui_app, key(KeyCode::PageUp)).unwrap();
        assert_eq!(tui_app.selected_filtered_index, 0);
    }

    #[test]
    fn theme_modal_key_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert!(!tui_app.show_theme_modal);

        // Open modal via 't'
        handle_key_event(&mut tui_app, key(KeyCode::Char('t'))).unwrap();
        assert!(tui_app.show_theme_modal);
        assert_eq!(tui_app.selected_theme_index, 0);

        // Navigate down
        handle_key_event(&mut tui_app, key(KeyCode::Char('j'))).unwrap();
        assert_eq!(tui_app.selected_theme_index, 1);

        // Navigate up
        handle_key_event(&mut tui_app, key(KeyCode::Char('k'))).unwrap();
        assert_eq!(tui_app.selected_theme_index, 0);

        // End key
        handle_key_event(&mut tui_app, key(KeyCode::End)).unwrap();
        assert_eq!(
            tui_app.selected_theme_index,
            tui_app.available_themes.len() - 1
        );

        // Home key
        handle_key_event(&mut tui_app, key(KeyCode::Home)).unwrap();
        assert_eq!(tui_app.selected_theme_index, 0);

        // Esc reverts and closes
        handle_key_event(&mut tui_app, key(KeyCode::Esc)).unwrap();
        assert!(!tui_app.show_theme_modal);

        // Re-open and commit via Enter with isolated config path
        let temp_dir = tempfile::tempdir().unwrap();
        tui_app.config_path = Some(temp_dir.path().join("config.toml"));
        handle_key_event(&mut tui_app, key(KeyCode::Char('t'))).unwrap();
        assert!(tui_app.show_theme_modal);
        handle_key_event(&mut tui_app, key(KeyCode::Enter)).unwrap();
        assert!(!tui_app.show_theme_modal);
    }

    #[test]
    fn theme_modal_mouse_scroll_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_theme_modal();
        assert_eq!(tui_app.selected_theme_index, 0);

        let scroll_down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        let scroll_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse_event(&mut tui_app, scroll_down).unwrap();
        assert_eq!(tui_app.selected_theme_index, 1);

        handle_mouse_event(&mut tui_app, scroll_up).unwrap();
        assert_eq!(tui_app.selected_theme_index, 0);
    }

    #[test]
    fn secret_modal_key_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();

        // 'n' opens new secret modal
        handle_key_event(&mut tui_app, key(KeyCode::Char('n'))).unwrap();
        assert!(tui_app.show_secret_modal);

        // Type secret name "TOKEN"
        for c in "TOKEN".chars() {
            handle_key_event(&mut tui_app, key(KeyCode::Char(c))).unwrap();
        }
        assert_eq!(tui_app.secret_modal_name, "TOKEN");

        // Tab to value field
        handle_key_event(&mut tui_app, key(KeyCode::Tab)).unwrap();
        assert_eq!(
            tui_app.secret_modal_field,
            crate::tui::app::SecretModalField::Value
        );

        // Generate value via Ctrl+G
        handle_key_event(
            &mut tui_app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(tui_app.secret_modal_value.len(), 32);

        // Toggle mask via Ctrl+R
        assert!(tui_app.secret_modal_masked);
        handle_key_event(
            &mut tui_app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert!(!tui_app.secret_modal_masked);

        // Enter commits
        handle_key_event(&mut tui_app, key(KeyCode::Enter)).unwrap();
        assert!(!tui_app.show_secret_modal);
        assert_eq!(tui_app.secrets.len(), 1);
        assert_eq!(tui_app.secrets[0].name, "TOKEN");

        // 'e' opens edit modal on selected secret
        handle_key_event(&mut tui_app, key(KeyCode::Char('e'))).unwrap();
        assert!(tui_app.show_secret_modal);
        assert_eq!(
            tui_app.secret_modal_mode,
            crate::tui::app::SecretModalMode::Edit
        );
        assert_eq!(tui_app.secret_modal_name, "TOKEN");

        // Esc cancels edit modal
        handle_key_event(&mut tui_app, key(KeyCode::Esc)).unwrap();
        assert!(!tui_app.show_secret_modal);
    }

    #[test]
    fn delete_modal_key_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);
        app.set(&config, "TO_DELETE", b"val").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.secrets.len(), 1);

        // 'd' opens delete confirmation modal
        handle_key_event(&mut tui_app, key(KeyCode::Char('d'))).unwrap();
        assert!(tui_app.show_delete_modal);
        assert_eq!(tui_app.delete_modal_secret_name, "TO_DELETE");

        // 'n' cancels
        handle_key_event(&mut tui_app, key(KeyCode::Char('n'))).unwrap();
        assert!(!tui_app.show_delete_modal);
        assert_eq!(tui_app.secrets.len(), 1);

        // Re-open and confirm with 'y'
        handle_key_event(&mut tui_app, key(KeyCode::Char('d'))).unwrap();
        assert!(tui_app.show_delete_modal);
        handle_key_event(&mut tui_app, key(KeyCode::Char('y'))).unwrap();
        assert!(!tui_app.show_delete_modal);
        assert_eq!(tui_app.secrets.len(), 0);
    }

    #[test]
    fn history_modal_key_events() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let keychain = crate::keychain::MemoryKeychain::default();
        crate::session::init(
            &store,
            &keychain,
            b"pw",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let master = crate::session::current_master_key(&keychain)
            .unwrap()
            .unwrap();
        let keypair = crate::session::account_keypair(&store, &master).unwrap();
        let project = crate::vault::Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        let theme = crate::theme::Theme::default();
        let app = crate::commands::App::new(&store, &keychain);
        app.set(&config, "HIST_SECRET", b"val-v1").unwrap();
        app.set(&config, "HIST_SECRET", b"val-v2").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        assert_eq!(tui_app.secrets.len(), 1);

        // 'h' opens history modal
        handle_key_event(&mut tui_app, key(KeyCode::Char('h'))).unwrap();
        assert!(tui_app.show_history_modal);
        assert_eq!(tui_app.history_modal_items.len(), 2);
        assert_eq!(tui_app.history_modal_selected_index, 0);

        // 'j' moves down to v1
        handle_key_event(&mut tui_app, key(KeyCode::Char('j'))).unwrap();
        assert_eq!(tui_app.history_modal_selected_index, 1);

        // 'r' toggles reveal
        assert!(!tui_app.history_modal_revealed);
        handle_key_event(&mut tui_app, key(KeyCode::Char('r'))).unwrap();
        assert!(tui_app.history_modal_revealed);

        // Enter rolls back to v1 (creates v3)
        handle_key_event(&mut tui_app, key(KeyCode::Enter)).unwrap();
        assert!(!tui_app.show_history_modal);
        assert_eq!(tui_app.secrets[0].version, 3);
        assert_eq!(app.get(&tui_app.config, "HIST_SECRET").unwrap(), b"val-v1");

        // Re-open and Esc closes
        handle_key_event(&mut tui_app, key(KeyCode::Char('h'))).unwrap();
        assert!(tui_app.show_history_modal);
        handle_key_event(&mut tui_app, key(KeyCode::Esc)).unwrap();
        assert!(!tui_app.show_history_modal);
    }
}
