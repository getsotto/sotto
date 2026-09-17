//! Test probe for the CLI spinner tests (see `tests/spinner.rs`).
//!
//! Every spinner in the real binary wraps an operation that needs an OS keychain or a sync
//! server, neither of which a test may require, so this probe stands in for a slow command: it
//! calls the same [`sotto_cli::feedback::spinner`] a command calls, holds it for a
//! caller-controlled duration, then writes data to stdout exactly as `sotto get` does.
//!
//! The hold time comes from `SPINNER_PROBE_HOLD_MS`, letting a test land on either side of the
//! spinner's visibility delay without changing production code or depending on machine speed.
//!
//! Not part of any shipped binary: an `examples/` target, like `e2e_seed`.

use std::io::Write;
use std::thread;
use std::time::Duration;

fn main() {
    let hold_ms: u64 = std::env::var("SPINNER_PROBE_HOLD_MS")
        .expect("SPINNER_PROBE_HOLD_MS must be set")
        .parse()
        .expect("SPINNER_PROBE_HOLD_MS must be an integer");
    // A test that checks the drop cleanup keeps the process alive past the visibility delay, so
    // a late frame from the spinner's worker would have somewhere to land.
    let linger_ms: u64 = std::env::var("SPINNER_PROBE_LINGER_MS")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .expect("SPINNER_PROBE_LINGER_MS must be an integer");

    // Markers bracket the operation on stderr, so a test can prove spinner frames land inside
    // the hold rather than before it or after the spinner has been dropped. The two streams are
    // captured separately, so no cross-stream timing is ever compared.
    eprintln!("probe:begin");
    {
        let _spinner = sotto_cli::feedback::spinner("Probe...");
        thread::sleep(Duration::from_millis(hold_ms));
    }
    eprintln!("probe:end");

    thread::sleep(Duration::from_millis(linger_ms));

    // Data goes to stdout only, once the operation is done.
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"probe-data").expect("write stdout");
    stdout.flush().expect("flush stdout");
}
