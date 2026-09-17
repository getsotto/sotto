//! Secure-ish clipboard hand-off with conditional expiry.
//!
//! The helper is a separate process because a Rust child thread is terminated when the CLI exits.
//! It owns the platform clipboard until the 45-second lifetime expires.

use std::io::{self, BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use arboard::Clipboard;
use zeroize::Zeroize;

use crate::error::{Error, Result};

pub const CLEAR_AFTER: Duration = Duration::from_secs(45);

/// Copy text and wait only for the helper's readiness acknowledgement. The timer runs in the
/// helper, so this function returns promptly and never holds a pipeline open.
pub fn copy(text: &str) -> Result<()> {
    if text.as_bytes().contains(&0) {
        return Err(Error::Input(
            "cannot copy text containing a NUL byte".into(),
        ));
    }
    let exe = std::env::current_exe().map_err(|e| Error::Io(e.to_string()))?;
    let mut child = Command::new(exe)
        .arg("__clipboard-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_PASSWORD")
        .env_remove("SOTTO_SECRET_KEY")
        .spawn()
        .map_err(|e| Error::Io(format!("starting clipboard helper: {e}")))?;

    if let Some(mut input) = child.stdin.take() {
        if let Err(error) = input.write_all(text.as_bytes()) {
            let _ = child.kill();
            return Err(Error::Io(error.to_string()));
        }
    }
    let output = match child.stdout.take() {
        Some(output) => output,
        None => {
            let _ = child.kill();
            return Err(Error::Io(
                "clipboard helper has no acknowledgement pipe".into(),
            ));
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = io::BufReader::new(output)
            .read_line(&mut line)
            .map(|_| line);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(line)) if line.trim() == "READY" => Ok(()),
        Ok(Ok(_)) => {
            let _ = child.kill();
            Err(Error::Io(
                "clipboard helper did not acknowledge the copy".into(),
            ))
        }
        Ok(Err(e)) => {
            let _ = child.kill();
            Err(Error::Io(format!("clipboard helper acknowledgement: {e}")))
        }
        Err(_) => {
            let _ = child.kill();
            Err(Error::Io("clipboard helper timed out".into()))
        }
    }
}

/// Entry point for the hidden helper command. It intentionally has no access to the vault or
/// server and emits only a secret-free readiness token.
pub fn run_helper() -> Result<()> {
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Io(e.to_string()))?;
    let mut text = zeroize::Zeroizing::new(String::from_utf8(bytes.to_vec()).map_err(|error| {
        let mut invalid = error.into_bytes();
        invalid.zeroize();
        Error::Input("clipboard requires valid UTF-8 text".into())
    })?);
    if text.as_bytes().contains(&0) {
        return Err(Error::Input(
            "cannot copy text containing a NUL byte".into(),
        ));
    }
    let mut clipboard =
        Clipboard::new().map_err(|e| Error::Io(format!("opening clipboard: {e}")))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| Error::Io(format!("writing clipboard: {e}")))?;
    io::stdout()
        .write_all(b"READY\n")
        .map_err(|e| Error::Io(e.to_string()))?;
    io::stdout().flush().ok();
    clear_after(&text, || std::thread::sleep(CLEAR_AFTER), &mut clipboard);
    text.zeroize();
    Ok(())
}

/// Clear only when the clipboard still contains the text written by this helper. Read failures
/// and replacement content are preserved. The immediate second read narrows the race between
/// observing our text and asking the backend to clear it.
pub fn clear_if_unchanged_with<G>(
    expected: &str,
    read: std::result::Result<String, ()>,
    mut clear: G,
) where
    G: FnMut() -> std::result::Result<(), ()>,
{
    let Ok(mut current) = read else {
        return;
    };
    let unchanged = current == expected;
    current.zeroize();
    if unchanged {
        let _ = clear();
    }
}

trait ClipboardOps {
    fn read_text(&mut self) -> std::result::Result<String, ()>;
    fn clear_text(&mut self) -> std::result::Result<(), ()>;
}

impl ClipboardOps for Clipboard {
    fn read_text(&mut self) -> std::result::Result<String, ()> {
        self.get_text().map_err(|_| ())
    }

    fn clear_text(&mut self) -> std::result::Result<(), ()> {
        self.clear().map_err(|_| ())
    }
}

fn clear_after<F, B>(expected: &str, wait: F, backend: &mut B)
where
    F: FnOnce(),
    B: ClipboardOps,
{
    wait();
    let Ok(mut observed) = backend.read_text() else {
        return;
    };
    let still_expected = observed == expected;
    observed.zeroize();
    if !still_expected {
        return;
    }
    let current = backend.read_text();
    clear_if_unchanged_with(expected, current, || backend.clear_text());
}

#[cfg(test)]
mod tests {
    use super::{clear_after, clear_if_unchanged_with, CLEAR_AFTER};

    #[test]
    fn clears_unchanged_text() {
        let mut cleared = false;
        clear_if_unchanged_with("secret", Ok("secret".into()), || {
            cleared = true;
            Ok(())
        });
        assert!(cleared);
    }

    #[test]
    fn preserves_replaced_or_unreadable_clipboard() {
        let mut cleared = false;
        clear_if_unchanged_with("secret", Ok("new text".into()), || {
            cleared = true;
            Ok(())
        });
        clear_if_unchanged_with("secret", Err(()), || {
            cleared = true;
            Ok(())
        });
        assert!(!cleared);
    }

    #[test]
    fn expiry_waits_before_attempting_conditional_clear() {
        assert_eq!(CLEAR_AFTER.as_secs(), 45);
        let mut waited = false;
        let mut backend = FakeClipboard {
            cleared: false,
            reads: vec![Ok("secret".into()), Ok("secret".into())],
        };
        clear_after("secret", || waited = true, &mut backend);
        assert!(waited);
        assert!(backend.cleared);
    }

    #[test]
    fn preserves_replacement_seen_during_recheck() {
        let mut backend = FakeClipboard {
            cleared: false,
            reads: vec![Ok("new text".into()), Ok("secret".into())],
        };
        clear_after("secret", || {}, &mut backend);
        assert!(!backend.cleared);
    }

    struct FakeClipboard {
        cleared: bool,
        reads: Vec<std::result::Result<String, ()>>,
    }

    impl super::ClipboardOps for FakeClipboard {
        fn read_text(&mut self) -> std::result::Result<String, ()> {
            self.reads.pop().unwrap_or_else(|| Ok("secret".into()))
        }

        fn clear_text(&mut self) -> std::result::Result<(), ()> {
            self.cleared = true;
            Ok(())
        }
    }
}
