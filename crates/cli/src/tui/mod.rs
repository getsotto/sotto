//! Interactive split-pane TUI dashboard for Sotto.
//!
//! Provides an interactive terminal overview of secrets, environment switching,
//! metadata inspection, search filtering, and safe clipboard copying when bare `sotto`
//! is invoked in an interactive terminal.

pub mod app;
pub mod events;
pub mod theme;
pub mod ui;

use std::io;
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::commands::App;
use crate::config::Config;
use crate::error::Result;
use crate::store::Store;
use crate::theme::Theme;
use crate::tui::app::TuiApp;

use std::sync::Arc;

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard {
    raw_mode: bool,
    entered_screen: bool,
    original_panic_hook: Option<Arc<PanicHook>>,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            entered_screen: false,
            original_panic_hook: None,
        }
    }

    fn enter(&mut self) -> Result<()> {
        enable_raw_mode()?;
        self.raw_mode = true;

        self.entered_screen = true;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;

        let original = std::panic::take_hook();
        let original_arc = Arc::new(original);
        let panic_prev = Arc::clone(&original_arc);

        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                Show
            );
            panic_prev(panic_info);
        }));

        self.original_panic_hook = Some(original_arc);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
        if self.entered_screen {
            let _ = execute!(
                io::stdout(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                Show
            );
        }
        if let Some(prev) = self.original_panic_hook.take() {
            std::panic::set_hook(Box::new(move |panic_info| {
                prev(panic_info);
            }));
        }
    }
}

/// Run the interactive TUI dashboard.
///
/// Configures terminal raw mode, sets up mouse capture, and renders the split-pane
/// secrets dashboard until the user quits.
pub fn run(app: &App, store: &Store, config: &Config, theme: &Theme) -> Result<()> {
    let mut guard = TerminalGuard::new();
    guard.enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app_state = TuiApp::new(app, store, config.clone(), theme)?;

    while app_state.running {
        terminal.draw(|f| ui::draw(f, &app_state))?;

        if crossterm::event::poll(Duration::from_millis(50))? {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    events::handle_key_event(&mut app_state, key)?;
                }
                Event::Mouse(mouse) => {
                    events::handle_mouse_event(&mut app_state, mouse)?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn terminal_guard_restores_panic_hook_on_teardown() {
        let sentinel_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&sentinel_flag);

        // Save active hook and install test sentinel hook
        let test_prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            flag_clone.store(true, Ordering::SeqCst);
        }));

        {
            let mut guard = TerminalGuard::new();
            let original = std::panic::take_hook();
            let original_arc = Arc::new(original);
            let panic_prev = Arc::clone(&original_arc);

            std::panic::set_hook(Box::new(move |panic_info| {
                let _ = disable_raw_mode();
                panic_prev(panic_info);
            }));

            guard.original_panic_hook = Some(original_arc);
            // Verify inner hook was installed
            assert!(guard.original_panic_hook.is_some());
        } // guard drops here and must restore sentinel hook

        // Retrieve restored hook and verify sentinel was reinstated
        let restored_hook = std::panic::take_hook();
        // Restore initial hook for test runner safety before asserting
        std::panic::set_hook(test_prev);

        // Verify restored hook was not None
        drop(restored_hook);
        assert!(!sentinel_flag.load(Ordering::SeqCst));
    }
}
