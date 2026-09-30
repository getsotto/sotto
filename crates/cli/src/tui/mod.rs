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

pub struct TerminalGuard {
    raw_mode: bool,
    entered_screen: bool,
    original_panic_hook: Option<Arc<PanicHook>>,
}

impl Default for TerminalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalGuard {
    pub fn new() -> Self {
        Self {
            raw_mode: false,
            entered_screen: false,
            original_panic_hook: None,
        }
    }

    pub fn enter(&mut self) -> Result<()> {
        enable_raw_mode()?;
        self.raw_mode = true;

        self.entered_screen = true;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;

        self.install_panic_hook(restore_terminal);
        Ok(())
    }

    /// Chain a hook that runs `restore` before the previous hook prints the panic. The restore
    /// step is a parameter so tests can observe the hook without driving a real terminal.
    pub fn install_panic_hook(&mut self, restore: impl Fn() + Send + Sync + 'static) {
        let original = std::panic::take_hook();
        let original_arc = Arc::new(original);
        let panic_prev = Arc::clone(&original_arc);

        std::panic::set_hook(Box::new(move |panic_info| {
            restore();
            panic_prev(panic_info);
        }));

        self.original_panic_hook = Some(original_arc);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    );
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
        // `set_hook` panics on a thread that is already panicking, and a panic in a destructor
        // during unwinding aborts the process. Mid-unwind the dashboard's hook has already
        // restored the terminal and the process is on its way out, so leave it installed.
        if std::thread::panicking() {
            return;
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
    use std::sync::{Mutex, PoisonError};

    /// The panic hook is process-wide and libtest runs tests on parallel threads, so two tests
    /// swapping it at once catch each other's panics and put back each other's hooks. Every test
    /// that touches the hook holds this lock.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` with a sentinel as the previous panic hook and report whether it ran. The
    /// runner's own hook is back before this returns, so a failing assertion afterwards still
    /// prints its message.
    fn sentinel_ran_during(body: impl FnOnce()) -> bool {
        let _serial = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_hook = Arc::clone(&ran);
        let runner_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| ran_in_hook.store(true, Ordering::SeqCst)));

        body();

        let _ = std::panic::take_hook();
        std::panic::set_hook(runner_hook);
        ran.load(Ordering::SeqCst)
    }

    /// A restore step that records whether the dashboard's panic hook called it.
    fn recording_restore() -> (Arc<AtomicBool>, impl Fn() + Send + Sync + 'static) {
        let restored = Arc::new(AtomicBool::new(false));
        let restored_in_hook = Arc::clone(&restored);
        (restored, move || {
            restored_in_hook.store(true, Ordering::SeqCst)
        })
    }

    #[test]
    fn terminal_guard_panic_hook_executes_and_delegates() {
        let (restored, restore) = recording_restore();

        let delegated = sentinel_ran_during(|| {
            let mut guard = TerminalGuard::new();
            guard.install_panic_hook(restore);
            let _ = std::panic::catch_unwind(|| panic!("dashboard panicked"));
        });

        assert!(
            restored.load(Ordering::SeqCst),
            "a panic in the dashboard must restore the terminal"
        );
        assert!(delegated, "the panic must still reach the previous hook");
    }

    #[test]
    fn terminal_guard_restores_panic_hook_on_drop() {
        let (restored, restore) = recording_restore();

        // The dashboard's hook forwards to the previous one, so the sentinel running proves
        // nothing on its own. The restore step not running is what shows the hook was removed.
        let reached_previous = sentinel_ran_during(|| {
            let mut guard = TerminalGuard::new();
            guard.install_panic_hook(restore);
            drop(guard);
            let _ = std::panic::catch_unwind(|| panic!("after the dashboard closed"));
        });

        assert!(
            !restored.load(Ordering::SeqCst),
            "a panic after the dashboard closed must not touch the terminal"
        );
        assert!(reached_previous, "the previous hook must handle it instead");
    }

    #[test]
    fn terminal_guard_dropped_by_a_panic_lets_it_unwind() {
        let (_, restore) = recording_restore();
        let mut outcome = None;

        // Without the `panicking()` check this never returns: dropping the guard mid-unwind
        // calls `set_hook` on a panicking thread and the whole test binary aborts.
        sentinel_ran_during(|| {
            outcome = Some(std::panic::catch_unwind(move || {
                let mut guard = TerminalGuard::new();
                guard.install_panic_hook(restore);
                panic!("dashboard panicked");
            }));
        });

        assert!(matches!(outcome, Some(Err(_))), "the panic must unwind");
    }
}
