//! Integration tests for bare `sotto` invocations and dashboard launch behavior.

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
        "bare non-interactive invocation must not initialize data dir"
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
}
