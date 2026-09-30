//! Test probe for terminal guard lifecycle and panic resilience under a pseudo terminal.
//!
//! Enters terminal guard (enabling raw mode, alternate screen, and panic hook),
//! then either exits normally to trigger teardown in drop or panics to trigger
//! teardown in the panic hook.
//!
//! Used by `crates/cli/tests/dashboard_cli.rs` without requiring an OS keychain.

use sotto_cli::tui::TerminalGuard;

fn main() {
    let mode = std::env::var("TERMINAL_GUARD_MODE").unwrap_or_else(|_| "lifecycle".to_string());
    let mut guard = TerminalGuard::new();
    guard.enter().expect("enter terminal guard");

    match mode.as_str() {
        "lifecycle" => {
            // Guard drops normally at end of main scope
        }
        "panic" => {
            panic!("intentional probe panic to test terminal restoration hook");
        }
        other => panic!("unknown terminal guard probe mode: {other}"),
    }
}
