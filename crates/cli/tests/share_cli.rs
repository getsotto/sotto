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
fn invalid_token_lifetimes_are_rejected_before_local_setup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    for value in ["0", "366", "4294967295"] {
        let data_dir = scratch.path().join(format!("days-{value}"));
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("SOTTO_PASSWORD")
            .args(["--plain", "token", "create", "--expires-in-days", value])
            .output()
            .expect("run sotto");

        assert_eq!(output.status.code(), Some(2), "accepted {value}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(stderr.contains("--expires-in-days"), "{value}: {stderr}");
        assert!(stderr.contains("1..=365"), "{value}: {stderr}");
        assert!(
            !stderr.contains("sotto.toml"),
            "{value} reached project lookup: {stderr}"
        );
        assert!(!data_dir.exists(), "{value} created {}", data_dir.display());
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
        termios.local_flags.remove(LocalFlags::ISIG);
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
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("wait for child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("process on pty did not exit within 60 seconds");
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

    fn setup_test_project_with_secret(scratch: &std::path::Path) -> Option<std::path::PathBuf> {
        let data_dir = scratch.join("sotto-data");

        let init_output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch)
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
                return None;
            }
        }

        assert_eq!(
            init_output.status.code(),
            Some(0),
            "init must succeed: {}",
            String::from_utf8_lossy(&init_output.stderr)
        );

        let set_output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch)
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .args(["set", "DEMO_SECRET", "--value", "super-secret"])
            .output()
            .expect("run sotto set");

        assert_eq!(
            set_output.status.code(),
            Some(0),
            "set must succeed: {}",
            String::from_utf8_lossy(&set_output.stderr)
        );

        Some(data_dir)
    }

    #[test]
    fn interactive_share_with_defaults_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .arg("share")
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Input: Enter to select DEMO_SECRET, Enter to select 1 view, Enter to select No expiry
        let run = run_on_pty_with_input(&mut cmd, b"\n\n\n");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            stderr.contains("Select a secret"),
            "expected secret selection prompt: {stderr}"
        );
        assert!(
            stderr.contains("Select view limit"),
            "expected view limit prompt: {stderr}"
        );
        assert!(
            stderr.contains("Select link lifetime"),
            "expected lifetime prompt: {stderr}"
        );
        assert!(
            stderr.contains("not logged in; run `sotto login`")
                || stderr.contains("share link")
                || stdout.contains("share link"),
            "expected share completion: stderr={stderr}, stdout={stdout}"
        );
    }

    #[test]
    fn interactive_share_cancels_at_secret_prompt_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .arg("share")
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send Escape to cancel at the first prompt (secret selection)
        let run = run_on_pty_with_input(&mut cmd, b"\x1b");
        assert!(run.status.success(), "cancelling prompt should exit with 0");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("aborted"),
            "expected aborted message: {stderr}"
        );
    }

    #[test]
    fn interactive_share_cancels_with_ctrl_c_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .arg("share")
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send 0x03 (Ctrl-C) to cancel at the first prompt
        let run = run_on_pty_with_input(&mut cmd, b"\x03");
        assert!(
            run.status.success(),
            "Ctrl-C cancellation should exit with 0"
        );
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("aborted"),
            "expected aborted message: {stderr}"
        );
    }

    #[test]
    fn interactive_share_cancels_at_views_prompt_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .arg("share")
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send Enter (select secret), then Escape (cancel views prompt)
        let run = run_on_pty_with_input(&mut cmd, b"\n\x1b");
        assert!(run.status.success(), "cancelling prompt should exit with 0");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("aborted"),
            "expected aborted message: {stderr}"
        );
    }

    #[test]
    fn interactive_share_cancels_at_lifetime_prompt_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .arg("share")
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send Enter (select secret), Enter (select view limit), then Escape (cancel lifetime prompt)
        let run = run_on_pty_with_input(&mut cmd, b"\n\n\x1b");
        assert!(run.status.success(), "cancelling prompt should exit with 0");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("aborted"),
            "expected aborted message: {stderr}"
        );
    }

    #[test]
    fn interactive_share_with_name_prompts_views_and_lifetime_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["share", "DEMO_SECRET"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // Send Enter (view limit), Enter (lifetime)
        let run = run_on_pty_with_input(&mut cmd, b"\n\n");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            !stderr.contains("Select a secret"),
            "should not prompt for secret name when supplied on CLI: {stderr}"
        );
        assert!(
            stderr.contains("Select view limit"),
            "expected view limit prompt: {stderr}"
        );
        assert!(
            stderr.contains("Select link lifetime"),
            "expected lifetime prompt: {stderr}"
        );
        assert!(
            stderr.contains("not logged in; run `sotto login`")
                || stderr.contains("share link")
                || stdout.contains("share link"),
            "expected share completion: stderr={stderr}, stdout={stdout}"
        );
    }

    #[test]
    fn interactive_share_skips_prompts_when_flags_provided_on_pty() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let Some(data_dir) = setup_test_project_with_secret(scratch.path()) else {
            return;
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
        cmd.current_dir(scratch.path())
            .args(["share", "DEMO_SECRET", "--views", "3", "--expire", "3600"])
            .env("SOTTO_DATA_DIR", &data_dir)
            .env("SOTTO_PASSWORD", "test-master-password")
            .env("TERM", "xterm-256color")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .env_remove("CI");

        // No input needed because all flags and arguments are provided
        let run = run_on_pty_with_input(&mut cmd, b"");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            !stderr.contains("Select a secret"),
            "should not prompt for secret name: {stderr}"
        );
        assert!(
            !stderr.contains("Select view limit"),
            "should not prompt for view limit: {stderr}"
        );
        assert!(
            !stderr.contains("Select link lifetime"),
            "should not prompt for lifetime: {stderr}"
        );
        assert!(
            stderr.contains("not logged in; run `sotto login`")
                || stderr.contains("share link")
                || stdout.contains("share link"),
            "expected share completion: stderr={stderr}, stdout={stdout}"
        );
    }
}

#[test]
fn env_use_omitted_name_precedes_project_discovery_non_interactively() {
    for (label, config) in [
        ("absent", None),
        ("malformed", Some("this is not = [valid toml")),
        (
            "valid",
            Some("project_id = \"00000000-0000-0000-0000-000000000000\"\nproject = \"test-project\"\nenvironment = \"dev\"\n"),
        ),
    ] {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let data_dir = scratch.path().join("sotto-data");
        if let Some(config) = config {
            std::fs::write(scratch.path().join("sotto.toml"), config).expect("write config");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env_remove("SOTTO_PASSWORD")
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_THEME")
            .args(["--plain", "env", "use"])
            .output()
            .expect("run sotto");
        assert_eq!(output.status.code(), Some(2), "{label}");
        assert!(output.stdout.is_empty(), "{label}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(stderr.contains("missing required argument <NAME>"), "{label}: {stderr}");
        assert!(!stderr.contains("Master password:"), "{label}: {stderr}");
    }
}

#[test]
fn env_use_supplied_name_still_discovers_project() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", scratch.path().join("sotto-data"))
        .env_remove("SOTTO_PASSWORD")
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_THEME")
        .args(["--plain", "env", "use", "dev"])
        .output()
        .expect("run sotto");
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("sotto.toml"), "{stderr}");
}
