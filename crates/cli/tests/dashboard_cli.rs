//! Integration tests for bare `sotto` invocations and dashboard launch behaviour.

use std::process::Command;

#[test]
fn non_interactive_bare_sotto_prints_help_to_stderr_and_exits_2() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let data_dir = scratch.path().join("sotto-data");

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", &data_dir)
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_THEME")
        .env_remove("SOTTO_PASSWORD")
        .output()
        .expect("run sotto");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("Usage: sotto"),
        "expected usage help on stderr: {stderr}"
    );
    assert!(
        stderr.contains("Commands:"),
        "expected commands listing on stderr: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty in non-interactive bare invocation"
    );
    assert!(
        !data_dir.exists(),
        "bare non-interactive invocation must not initialise data dir"
    );
}

#[test]
fn bare_sotto_with_help_exits_0() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let data_dir = scratch.path().join("sotto-data");

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", &data_dir)
        .arg("--help")
        .output()
        .expect("run sotto --help");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(
        stdout.contains("Usage: sotto"),
        "expected usage help on stdout: {stdout}"
    );
}

#[cfg(unix)]
mod pty_tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::path::PathBuf;
    use std::process::{Child, Stdio};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::pty::{openpty, Winsize};
    use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg};

    #[allow(dead_code)]
    struct PtyRun {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        termios_after_exit: Option<nix::sys::termios::Termios>,
    }

    fn run_on_pty_with_input(command: &mut Command, input: &[u8]) -> PtyRun {
        let ws = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let stdout_pty = openpty(Some(&ws), None).expect("openpty for stdout");
        let stderr_pty = openpty(Some(&ws), None).expect("openpty for stderr");
        no_echo_no_opost(&stdout_pty.slave);
        no_echo_no_opost(&stderr_pty.slave);

        command
            .stdin(Stdio::from(File::from(
                stdout_pty.slave.try_clone().expect("clone pty slave"),
            )))
            .stdout(Stdio::from(File::from(
                stdout_pty.slave.try_clone().expect("clone pty slave"),
            )))
            .stderr(Stdio::from(File::from(
                stderr_pty.slave.try_clone().expect("clone pty slave"),
            )));

        if !input.is_empty() {
            let mut master_file = File::from(stdout_pty.master.try_clone().expect("clone master"));
            master_file.write_all(input).expect("write input to pty");
        }

        let slave_check = stdout_pty.slave.try_clone().expect("clone slave pty");

        let mut child = command.spawn().expect("spawn on a pseudo terminal");

        drop(stdout_pty.slave);
        drop(stderr_pty.slave);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let stdout_reader = read_master(stdout_pty.master);
        let stderr_reader = read_master(stderr_pty.master);
        let status = wait_bounded(&mut child);
        let termios_after_exit = tcgetattr(&slave_check).ok();
        drop(slave_check);

        PtyRun {
            status,
            stdout: stdout_reader.join().expect("stdout reader"),
            stderr: stderr_reader.join().expect("stderr reader"),
            termios_after_exit,
        }
    }

    fn no_echo_no_opost(slave: &OwnedFd) {
        let mut termios = tcgetattr(slave).expect("tcgetattr");
        termios.local_flags.remove(LocalFlags::ECHO);
        termios.output_flags.remove(OutputFlags::OPOST);
        tcsetattr(slave, SetArg::TCSANOW, &termios).expect("tcsetattr");
    }

    fn read_master(master: OwnedFd) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut file = File::from(master);
            let mut collected = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => collected.extend_from_slice(&buffer[..read]),
                    Err(error) if error.raw_os_error() == Some(Errno::EIO as i32) => break,
                    Err(error) => panic!("reading pseudo terminal master: {error}"),
                }
            }
            collected
        })
    }

    fn wait_bounded(child: &mut Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().expect("wait for child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("process on pty did not exit within 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn bare_sotto_under_dumb_term_on_pty_prints_help_and_exits_2() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "dumb")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert_eq!(run.status.code(), Some(2));
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("Usage: sotto"),
            "expected usage on stderr under TERM=dumb: {stderr}"
        );
    }

    #[test]
    fn bare_sotto_on_pty_outside_project_reports_no_config() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert_eq!(run.status.code(), Some(3));
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("no sotto.toml found"),
            "expected no config error: {stderr}"
        );
    }

    #[test]
    fn bare_sotto_on_pty_without_identity_reports_no_identity() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert_eq!(run.status.code(), Some(4));
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("no identity"),
            "expected no identity error: {stderr}"
        );
    }

    fn guard_probe_binary() -> PathBuf {
        static PROBE: OnceLock<PathBuf> = OnceLock::new();
        PROBE
            .get_or_init(|| {
                let binary = PathBuf::from(env!("CARGO_BIN_EXE_sotto"));
                let profile_dir = binary.parent().expect("profile directory").to_path_buf();
                let probe = profile_dir.join("examples").join("terminal_guard_probe");
                let target_dir = profile_dir.parent().expect("target directory");
                let profile = profile_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("profile name");
                let profile = if profile == "debug" { "dev" } else { profile };
                let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
                let output = Command::new(cargo)
                    .args([
                        "build",
                        "--quiet",
                        "--example",
                        "terminal_guard_probe",
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
                    "cargo build --example terminal_guard_probe failed: {}",
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

    #[test]
    fn terminal_guard_on_pty_restores_screen_and_raw_mode_on_drop() {
        let mut cmd = Command::new(guard_probe_binary());
        cmd.env("TERMINAL_GUARD_MODE", "lifecycle");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert_eq!(run.status.code(), Some(0));

        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            stdout.contains("\x1b[?1049h"),
            "expected alternate screen enter sequence \\x1b[?1049h: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?1049l"),
            "expected alternate screen leave sequence \\x1b[?1049l: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?25l"),
            "expected cursor hide sequence \\x1b[?25l: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?25h"),
            "expected cursor show sequence \\x1b[?25h: {stdout}"
        );

        let termios = run
            .termios_after_exit
            .expect("slave pty termios after child exit");
        assert!(
            termios.local_flags.contains(LocalFlags::ICANON),
            "expected ICANON flag restored after terminal guard drop"
        );
    }

    #[test]
    fn terminal_guard_on_pty_restores_screen_and_raw_mode_on_panic() {
        let mut cmd = Command::new(guard_probe_binary());
        cmd.env("TERMINAL_GUARD_MODE", "panic");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert!(!run.status.success(), "expected probe to fail via panic");

        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            stdout.contains("\x1b[?1049l"),
            "expected alternate screen leave sequence \\x1b[?1049l on panic: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?25h"),
            "expected cursor show sequence \\x1b[?25h on panic: {stdout}"
        );

        let termios = run
            .termios_after_exit
            .expect("slave pty termios after child panic");
        assert!(
            termios.local_flags.contains(LocalFlags::ICANON),
            "expected ICANON flag restored by panic hook"
        );
    }

    #[test]
    fn bare_sotto_on_pty_launches_dashboard_and_restores_terminal_on_quit() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        // Initialise a valid sotto project and identity non-interactively
        let init_output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .arg("init")
            .output()
            .expect("run sotto init");

        if init_output.status.code() == Some(5) {
            let stderr = String::from_utf8_lossy(&init_output.stderr);
            if stderr.contains("Platform secure storage failure")
                || stderr.contains("org.freedesktop.secrets")
                || stderr.contains("keychain error")
            {
                eprintln!("skipping: OS keychain not available in headless environment");
                return;
            }
        }

        assert_eq!(
            init_output.status.code(),
            Some(0),
            "init must succeed: {}",
            String::from_utf8_lossy(&init_output.stderr)
        );

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send 'q' to quit dashboard immediately upon launch
        let run = run_on_pty_with_input(&mut cmd, b"q");
        assert_eq!(run.status.code(), Some(0));

        let stdout = String::from_utf8_lossy(&run.stdout);
        // Alternate screen entered and left
        assert!(
            stdout.contains("\x1b[?1049h"),
            "expected alternate screen enter sequence \\x1b[?1049h in stdout: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?1049l"),
            "expected alternate screen leave sequence \\x1b[?1049l in stdout: {stdout}"
        );
        // Cursor hidden and shown
        assert!(
            stdout.contains("\x1b[?25l"),
            "expected cursor hide sequence \\x1b[?25l in stdout: {stdout}"
        );
        assert!(
            stdout.contains("\x1b[?25h"),
            "expected cursor show sequence \\x1b[?25h in stdout: {stdout}"
        );

        // Verify canonical mode (raw mode disabled) was restored on slave pty
        let termios = run
            .termios_after_exit
            .expect("slave pty termios after child exit");
        assert!(
            termios.local_flags.contains(LocalFlags::ICANON),
            "expected ICANON flag restored after dashboard exit"
        );
    }
}
