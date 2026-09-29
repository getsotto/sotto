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
        KeyCode::Char('c') => {
            app.copy_selected()?;
        }
        KeyCode::Char('r') => {
            app.toggle_reveal()?;
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
}
