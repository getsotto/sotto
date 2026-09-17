//! Stream-discipline tests for the delayed CLI spinner (`src/feedback.rs`).
//!
//! The contract: progress goes to stderr, data only to stdout, and a redirected or
//! non-interactive stream carries no spinner frames and no ANSI escapes. `feedback.rs` unit
//! tests the pure gate; these tests run the production spinner in a real process so the delayed
//! draw and the cleanup on drop are covered as well.
//!
//! A pseudo terminal is the only way to make a child's streams interactive: a child whose
//! stdin, stdout, and stderr are PTY slaves passes the `IsTerminal` gate exactly as a user's
//! terminal would. Two PTYs keep the streams separable - one for stdin/stdout, one for stderr -
//! and the slave ends are put into raw-ish mode (no echo, no output post-processing) so the
//! captured bytes are the bytes the process actually wrote. This needs no desktop session and
//! works on a headless CI runner.
//!
//! No command in the shipping binary reaches a spinner without an OS keychain or a sync server:
//! every `feedback::spinner` call site sits behind `ensure_unlocked` or `sync_client`. The PTY
//! cases therefore drive `examples/spinner_probe.rs`, which calls the same production API with a
//! hold time the test controls. The piped case also runs the real binary on a command that needs
//! neither the keychain nor the network, pinning its data-only stdout.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::pty::{openpty, Winsize};
use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg};

/// The production tick set: any of these on a stream means a frame was drawn.
const SPINNER_GLYPHS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Well past the 200 ms visibility delay, leaving room for several 90 ms ticks.
const SLOW_HOLD_MS: &str = "800";

/// Well inside the visibility delay: a spinner must never draw for this operation.
const FAST_HOLD_MS: &str = "20";

struct PtyRun {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[test]
fn interactive_run_draws_the_spinner_on_stderr_and_keeps_stdout_data_only() {
    let run = run_on_pty(&mut probe_command(SLOW_HOLD_MS));
    assert!(
        run.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The probe writes no newline, so the exact bytes still pin "data only" even though the CLI
    // appends one when stdout is a terminal (see `write_value`).
    assert_eq!(run.stdout, b"probe-data");

    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    let begin = stderr.find("probe:begin").expect("begin marker");
    let end = stderr.find("probe:end").expect("end marker");
    let frame = stderr
        .find(|c| SPINNER_GLYPHS.contains(&c))
        .expect("at least one spinner frame on the PTY stderr");
    assert!(
        begin < frame && frame < end,
        "the frame must land inside the operation: {stderr:?}"
    );
    assert!(
        stderr.contains("Probe..."),
        "the frame must carry the label: {stderr:?}"
    );
}

#[test]
fn piped_probe_carries_no_spinner_frames_or_ansi_on_either_stream() {
    let output = piped(probe_command(SLOW_HOLD_MS));
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exact bytes both ways: the hold is long enough that a spinner would have drawn on a
    // terminal, so a marker-only stderr pins the delayed frame never appearing when redirected.
    assert_eq!(output.stdout, b"probe-data");
    assert_eq!(output.stderr, b"probe:begin\nprobe:end\n");
    assert!(!has_ansi(&output.stdout) && !has_ansi(&output.stderr));
}

#[test]
fn piped_real_binary_emits_exact_bytes_with_no_spinner_or_ansi() {
    // `sotto get KEY | cat` is the shape under test, but `get` reaches for the OS keychain
    // before it can produce any bytes, and a headless runner has none. `theme current` is the
    // same shape - one data value on stdout, nothing else - without the keychain or the network.
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["theme", "current"])
        .env("SOTTO_DATA_DIR", data_dir.path())
        .env_remove("SOTTO_THEME")
        .env_remove("SOTTO_TOKEN")
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sotto");

    assert!(
        output.status.success(),
        "sotto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"nord\n");
    assert_eq!(output.stderr, b"");
    assert!(!has_ansi(&output.stdout) && !has_ansi(&output.stderr));
}

#[test]
fn fast_operation_leaves_no_spinner_frame_behind() {
    // The probe lingers well past the 200 ms delay after dropping the spinner, so a late worker
    // frame would have somewhere to land: an immediate exit could hide the regression.
    let mut command = probe_command(FAST_HOLD_MS);
    command.env("SPINNER_PROBE_LINGER_MS", "500");
    let run = run_on_pty(&mut command);
    assert!(
        run.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert_eq!(run.stdout, b"probe-data");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        stderr.contains("probe:begin") && stderr.contains("probe:end"),
        "the operation must complete: {stderr:?}"
    );
    assert!(
        !stderr.contains(|c| SPINNER_GLYPHS.contains(&c)),
        "a delayed frame leaked after a fast operation: {stderr:?}"
    );
    assert!(
        !has_ansi(&run.stderr),
        "a fast operation must write nothing at all: {stderr:?}"
    );
}

#[test]
fn plain_flag_suppresses_the_spinner_in_an_interactive_run() {
    let mut command = probe_command(SLOW_HOLD_MS);
    let run = run_on_pty(command.arg("--plain"));
    assert!(
        run.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert_eq!(run.stdout, b"probe-data");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        stderr.contains("probe:begin") && stderr.contains("probe:end"),
        "the operation must complete: {stderr:?}"
    );
    assert!(
        !stderr.contains(|c| SPINNER_GLYPHS.contains(&c)),
        "--plain must keep the spinner off even on a terminal: {stderr:?}"
    );
}

#[test]
fn ci_environment_suppresses_the_spinner_even_under_a_pty() {
    // CI runners can hand a process a PTY; the gate must stay closed there regardless.
    let mut command = probe_command(SLOW_HOLD_MS);
    command.env("CI", "true");
    let run = run_on_pty(&mut command);
    assert!(
        run.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert_eq!(run.stdout, b"probe-data");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        !stderr.contains(|c| SPINNER_GLYPHS.contains(&c)),
        "CI must keep the spinner off even on a terminal: {stderr:?}"
    );
}

/// The probe with a hermetic environment: no ambient CI flag or colour preference, and a TERM
/// the gate accepts.
fn probe_command(hold_ms: &str) -> Command {
    let mut command = Command::new(probe_binary());
    command
        .env("SPINNER_PROBE_HOLD_MS", hold_ms)
        .env("TERM", "xterm-256color")
        .env_remove("CI")
        .env_remove("NO_COLOR");
    command
}

/// The probe is an example binary, which `cargo test` builds alongside the test targets. A
/// filtered `cargo test --test spinner` does not build examples, and a stale probe would test
/// the previous library, so build it through cargo every run: cargo then decides whether the
/// example needs recompiling, and the probe always matches the library under test.
fn probe_binary() -> PathBuf {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let binary = PathBuf::from(env!("CARGO_BIN_EXE_sotto"));
            let profile_dir = binary.parent().expect("profile directory").to_path_buf();
            let probe = profile_dir.join("examples").join("spinner_probe");
            let target_dir = profile_dir.parent().expect("target directory");
            let profile = profile_dir
                .file_name()
                .and_then(|name| name.to_str())
                .expect("profile name");
            // Cargo names the dev profile's output directory `debug`, but `--profile` wants `dev`.
            let profile = if profile == "debug" { "dev" } else { profile };
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let output = Command::new(cargo)
                .args([
                    "build",
                    "--quiet",
                    "--example",
                    "spinner_probe",
                    "--profile",
                ])
                .arg(profile)
                .arg("--target-dir")
                .arg(target_dir)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("build the probe example");
            assert!(
                output.status.success(),
                "cargo build --example spinner_probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                probe.is_file(),
                "probe example missing at {}",
                probe.display()
            );
            probe
        })
        .clone()
}

/// Run a command with stdin/stdout on one PTY and stderr on another: every stream is a terminal
/// for the gate, yet stdout and stderr stay separately readable.
fn run_on_pty(command: &mut Command) -> PtyRun {
    let stdout_pty = openpty(Some(&winsize()), None).expect("openpty for stdout");
    let stderr_pty = openpty(Some(&winsize()), None).expect("openpty for stderr");
    no_echo_no_opost(&stdout_pty.slave);
    no_echo_no_opost(&stderr_pty.slave);

    command
        .stdin(slave_stdio(&stdout_pty.slave))
        .stdout(slave_stdio(&stdout_pty.slave))
        .stderr(slave_stdio(&stderr_pty.slave));
    let mut child = command.spawn().expect("spawn on a pseudo terminal");

    // The parent's copies of the slave ends must go, or the master reads never end: the
    // `Command` keeps every `Stdio` it was given alive until it is replaced or dropped.
    drop(stdout_pty.slave);
    drop(stderr_pty.slave);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let stdout_reader = read_master(stdout_pty.master);
    let stderr_reader = read_master(stderr_pty.master);
    let status = wait_bounded(&mut child);

    PtyRun {
        status,
        stdout: stdout_reader.join().expect("stdout reader"),
        stderr: stderr_reader.join().expect("stderr reader"),
    }
}

fn piped(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run piped")
}

fn winsize() -> Winsize {
    Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn slave_stdio(slave: &OwnedFd) -> Stdio {
    Stdio::from(File::from(slave.try_clone().expect("clone pty slave")))
}

/// Turn off echo and output post-processing on the slave, so the captured bytes are exactly what
/// the process wrote (a terminal would otherwise echo input and rewrite `\n` as `\r\n`).
fn no_echo_no_opost(slave: &OwnedFd) {
    let mut termios = tcgetattr(slave).expect("tcgetattr");
    termios.local_flags.remove(LocalFlags::ECHO);
    termios.output_flags.remove(OutputFlags::OPOST);
    tcsetattr(slave, SetArg::TCSANOW, &termios).expect("tcsetattr");
}

fn read_master(master: OwnedFd) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut file = File::from(master);
        let mut collected = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => collected.extend_from_slice(&buffer[..read]),
                // Linux reports EIO on a master read once every slave end has closed; that is
                // the end of the stream, not a failure.
                Err(error) if error.raw_os_error() == Some(Errno::EIO as i32) => break,
                Err(error) => panic!("reading pseudo terminal master: {error}"),
            }
        }
        collected
    })
}

/// Wait with a generous bound so a hung probe fails the test instead of hanging CI.
fn wait_bounded(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("wait for child") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("probe did not exit within 60 seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn has_ansi(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
}
