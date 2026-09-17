//! Terminal-only progress feedback for slow CLI operations.

use std::io::{self, IsTerminal};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// A delayed spinner which never writes to stdout. The delay prevents a fast operation from
/// flashing a frame, while the cancellation flag prevents a late worker from drawing after the
/// operation has completed.
pub struct Spinner {
    bar: Option<ProgressBar>,
    alive: Arc<AtomicBool>,
}

impl Spinner {
    pub fn new(label: &str) -> Self {
        if !enabled() {
            return Self {
                bar: None,
                alive: Arc::new(AtomicBool::new(false)),
            };
        }

        let bar = ProgressBar::with_draw_target(Some(0), ProgressDrawTarget::hidden());
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg}")
                .expect("static spinner template")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        bar.set_message(label.to_string());
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = Arc::clone(&alive);
        let worker_bar = bar.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            if worker_alive.load(Ordering::Acquire) {
                worker_bar.set_draw_target(ProgressDrawTarget::stderr());
                worker_bar.enable_steady_tick(Duration::from_millis(90));
            }
        });
        Self {
            bar: Some(bar),
            alive,
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(bar) = self.bar.take() {
            bar.disable_steady_tick();
            bar.finish_and_clear();
        }
    }
}

/// Progress is shown only for an interactive, styled session. In particular, a redirected
/// stdout or stderr must remain byte-clean for scripts and logs.
pub fn enabled() -> bool {
    allowed(
        std::env::args().any(|arg| arg == "--plain"),
        std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty()),
        crate::theme::ci_enabled(std::env::var("CI").ok().as_deref()),
        std::env::var("TERM").is_ok_and(|term| term == "dumb"),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    )
}

pub fn allowed(
    plain: bool,
    no_color: bool,
    ci: bool,
    dumb_terminal: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> bool {
    !plain && !no_color && !ci && !dumb_terminal && stdin_tty && stdout_tty && stderr_tty
}

pub fn spinner(label: &str) -> Spinner {
    Spinner::new(label)
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn progress_requires_every_interactive_gate() {
        assert!(allowed(false, false, false, false, true, true, true));
        for gate in 0..7 {
            let mut values = [false; 7];
            values[4..].fill(true);
            values[gate] = gate < 4;
            assert!(!allowed(
                values[0], values[1], values[2], values[3], values[4], values[5], values[6]
            ));
        }
    }
}
