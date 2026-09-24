use std::process::Command;

#[test]
fn invalid_share_limits_are_rejected_before_local_setup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    for option in [
        "--views=0",
        "--views=-1",
        "--views=101",
        "--expire=0",
        "--expire=-1",
        "--expire=2592001",
    ] {
        let data_dir = scratch
            .path()
            .join(option.trim_start_matches('-').replace('=', "-"));
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .arg("share")
            .arg("DEMO")
            .arg(option)
            .output()
            .expect("run sotto");

        assert!(!output.status.success(), "accepted {option}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        let (name, range) = if option.starts_with("--views") {
            ("--views", "1..=100")
        } else {
            ("--expire", "1..=2592000")
        };
        assert!(stderr.contains(name), "{option}: {stderr}");
        assert!(stderr.contains(range), "{option}: {stderr}");
        assert!(
            !data_dir.exists(),
            "{option} created {}",
            data_dir.display()
        );
    }
}

#[test]
fn invalid_rollback_versions_are_rejected_before_local_setup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    for (label, args) in [
        ("zero", vec!["rollback", "DEMO", "0"]),
        ("negative", vec!["rollback", "DEMO", "--", "-1"]),
    ] {
        let data_dir = scratch.path().join(label);
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("SOTTO_PASSWORD")
            .args(&args)
            .output()
            .expect("run sotto");

        assert!(!output.status.success(), "accepted {label}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains("<VERSION>"),
            "{label} should name the version argument: {stderr}"
        );
        assert!(
            stderr.contains("1..9223372036854775807"),
            "{label} should state the positive range: {stderr}"
        );
        assert!(!data_dir.exists(), "{label} created {}", data_dir.display());
    }

    // Positive parsing control: version 1 still reaches local-setup (missing project).
    let data_dir = scratch.path().join("positive");
    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", &data_dir)
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_THEME")
        .env_remove("SOTTO_PASSWORD")
        .args(["rollback", "DEMO", "1"])
        .output()
        .expect("run sotto");
    assert!(!output.status.success(), "expected missing-project failure");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("sotto.toml") || stderr.contains("config"),
        "positive control: {stderr}"
    );
}

#[test]
fn omitted_arguments_in_non_interactive_mode_fail_with_clear_error() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let data_dir = scratch.path().join("sotto-data");

    // Write project config directly so no OS keychain is needed (keeps tests portable across headless CI).
    std::fs::write(
        scratch.path().join("sotto.toml"),
        "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
    )
    .expect("write sotto.toml");

    // When session is locked and SOTTO_PASSWORD is unset, omitted secret name
    // must fail fast with missing-argument error without prompting for master password on stdin.
    for cmd in ["get", "rm", "share"] {
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .args([cmd])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env_remove("SOTTO_PASSWORD")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .output()
            .expect("run sotto");
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains("missing required argument <NAME>"),
            "stderr for {cmd} should mention missing argument: {stderr}"
        );
        assert!(
            !stderr.contains("Master password:"),
            "stderr for {cmd} should not attempt to prompt for master password: {stderr}"
        );
    }

    // Even with password supplied, non-interactive omitted arguments still fail with clear error.
    for cmd in ["get", "rm", "share"] {
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .args([cmd])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-password-123")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .output()
            .expect("run sotto");
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains("missing required argument <NAME>"),
            "stderr for {cmd} with password should mention missing argument: {stderr}"
        );
    }

    // sotto env use without name in non-interactive session
    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .args(["env", "use"])
        .env("SOTTO_DATA_DIR", &data_dir)
        .env("SOTTO_PASSWORD", "test-password-123")
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_THEME")
        .output()
        .expect("run sotto");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("missing required argument <NAME>"),
        "env use without arg stderr: {stderr}"
    );
}

#[cfg(unix)]
mod pty_tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::pty::{openpty, Winsize};
    use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg};

    #[allow(dead_code)]
    struct PtyRun {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
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

        PtyRun {
            status,
            stdout: stdout_reader.join().expect("stdout reader"),
            stderr: stderr_reader.join().expect("stderr reader"),
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
    fn interactive_env_use_on_pty_selects_environment() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let store = sotto_cli::store::Store::open(&data_dir.join("store.db")).expect("open store");
        store
            .create_project_with_id("00000000-0000-0000-0000-000000000000", "test-project")
            .expect("create project");
        store
            .create_environment("env-1", "00000000-0000-0000-0000-000000000000", "dev", b"")
            .expect("create dev env");
        store
            .create_environment("env-2", "00000000-0000-0000-0000-000000000000", "prod", b"")
            .expect("create prod env");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["env", "use"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send newline (Enter) to accept the first choice
        let run = run_on_pty_with_input(&mut cmd, b"\n");
        assert!(
            run.status.success(),
            "env use failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("active environment: dev"),
            "expected success message: {stderr}"
        );
    }

    #[test]
    fn interactive_env_use_on_pty_cancels_with_escape() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let store = sotto_cli::store::Store::open(&data_dir.join("store.db")).expect("open store");
        store
            .create_project_with_id("00000000-0000-0000-0000-000000000000", "test-project")
            .expect("create project");
        store
            .create_environment("env-1", "00000000-0000-0000-0000-000000000000", "dev", b"")
            .expect("create dev env");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["env", "use"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send 0x1b (Escape) to cancel the selection
        let run = run_on_pty_with_input(&mut cmd, b"\x1b");
        assert!(run.status.success(), "cancelling prompt should exit with 0");
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("aborted"),
            "expected aborted message: {stderr}"
        );
    }

    #[test]
    fn interactive_prompt_allowed_under_no_color_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let store = sotto_cli::store::Store::open(&data_dir.join("store.db")).expect("open store");
        store
            .create_project_with_id("00000000-0000-0000-0000-000000000000", "test-project")
            .expect("create project");
        store
            .create_environment("env-1", "00000000-0000-0000-0000-000000000000", "dev", b"")
            .expect("create dev env");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["env", "use"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send newline (Enter) to accept choice; under NO_COLOR it must prompt rather than fail
        let run = run_on_pty_with_input(&mut cmd, b"\n");
        assert!(
            run.status.success(),
            "NO_COLOR should permit prompt: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }

    #[test]
    fn interactive_prompt_rejected_under_dumb_term_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["env", "use"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "dumb")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert_eq!(run.status.code(), Some(2));
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains("missing required argument <NAME>"),
            "expected refusal under dumb term: {stderr}"
        );
    }

    #[test]
    fn interactive_get_no_copy_on_pty_fails_fast_without_prompt() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");

        std::fs::write(
            scratch.path().join("sotto.toml"),
            "project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n",
        )
        .expect("write sotto.toml");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["get", "--no-copy"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        let run = run_on_pty_with_input(&mut cmd, b"");
        assert!(!run.status.success());
        let stderr = String::from_utf8(run.stderr).expect("utf-8 stderr");
        assert!(
            stderr.contains(
                "refusing to print a secret to a terminal; use --reveal or pipe the output"
            ),
            "expected refusal: {stderr}"
        );
    }
}
