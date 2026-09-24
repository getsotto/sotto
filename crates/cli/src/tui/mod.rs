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

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            Show
        );
    }
}

/// Run the interactive TUI dashboard.
///
/// Configures terminal raw mode, sets up mouse capture, and renders the split-pane
/// secrets dashboard until the user quits.
pub fn run(app: &App, store: &Store, config: &Config, theme: &Theme) -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;

    let _guard = TerminalGuard;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            Show
        );
        original_hook(panic_info);
    }));

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
