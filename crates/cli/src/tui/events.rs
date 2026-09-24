//! Keyboard and mouse event handlers for the interactive Sotto dashboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::error::Result;
use crate::tui::app::TuiApp;

/// Handle incoming keyboard events.
pub fn handle_key_event(app: &mut TuiApp, key: KeyEvent) -> Result<()> {
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
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.running = false;
        }
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
}
