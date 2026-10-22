use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn valid_project_task_file_can_be_validated_without_opening_the_store() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        r#"version = 1

[[task]]
name = "test"
description = "Run the test suite"
executable = "cargo"
args = ["test"]
environments = ["dev"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .arg("task")
        .arg("validate")
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "valid: 1 task\n");
    assert!(
        !data.exists(),
        "validation must not initialise local secret state"
    );
}

#[test]
fn invalid_task_file_reports_its_path_without_opening_the_store() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    let manifest = project.join("sotto.tasks.toml");
    fs::write(
        &manifest,
        "version = 2\n\n[[task]]\nname = \"test\"\ndescription = \"Run tests\"\nexecutable = \"cargo\"\nenvironments = [\"dev\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "validate"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&manifest.display().to_string()), "{stderr}");
    assert!(
        stderr.contains("unsupported task file version 2"),
        "{stderr}"
    );
    assert!(
        !data.exists(),
        "validation must not initialise local secret state"
    );
}

#[test]
fn malformed_task_file_does_not_echo_source_text() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        "version = 1\n\n[[task]]\nname = \"test\"\ndescription = \"do_not_echo_this_secret\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "validate"])
        .current_dir(&project)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("sotto.tasks.toml:"), "{stderr}");
    assert!(!stderr.contains("do_not_echo_this_secret"), "{stderr}");
}

#[test]
fn missing_task_file_has_creation_guidance_without_opening_the_store() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "validate"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("sotto.tasks.toml"), "{stderr}");
    assert!(stderr.contains("create this file"), "{stderr}");
    assert!(
        !data.exists(),
        "validation must not initialise local secret state"
    );
}

#[test]
fn task_approval_refuses_non_interactive_confirmation_before_creating_local_state() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        "version = 1\n\n[[task]]\nname = \"test\"\ndescription = \"Run tests\"\nexecutable = \"cargo\"\nargs = [\"test\"]\nenvironments = [\"dev\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "approve", "test", "--env", "dev"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("interactive terminal"), "{stderr}");
    assert!(
        !data.exists(),
        "approval must not create local state before terminal confirmation"
    );
}

#[test]
fn task_ls_lists_multiple_declared_tasks_in_deterministic_order() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        r#"version = 1

[[task]]
name = "test"
description = "Run the test suite"
executable = "cargo"
args = ["test"]
environments = ["dev"]

[[task]]
name = "build"
description = "Build the project"
executable = "cargo"
args = ["build"]
environments = ["dev", "prod"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "ls"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected = "\
build: Build the project\n  environments: dev, prod\ntest: Run the test suite\n  environments: dev\n";
    assert_eq!(stdout, expected);
    assert!(
        !data.exists(),
        "task ls must not initialise local secret state"
    );
}

#[test]
fn task_ls_works_from_nested_directory() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    let nested = project.join("deep/nested/subdir");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        r#"version = 1

[[task]]
name = "lint"
description = "Run linter"
executable = "cargo"
args = ["clippy"]
environments = ["dev"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "ls"])
        .current_dir(&nested)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("lint: Run linter"), "{stdout}");
    assert!(stdout.contains("environments: dev"), "{stdout}");
    assert!(!data.exists());
}

#[test]
fn task_ls_lists_tasks_even_when_executable_is_unavailable() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    fs::write(
        project.join("sotto.tasks.toml"),
        r#"version = 1

[[task]]
name = "custom"
description = "A custom task with missing binary"
executable = "this-binary-definitely-does-not-exist-12345"
environments = ["dev"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "ls"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("custom: A custom task with missing binary"),
        "{stdout}"
    );
    assert!(stdout.contains("environments: dev"), "{stdout}");
    assert!(!data.exists());
}

#[test]
fn task_ls_fails_when_manifest_is_missing() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "ls"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("sotto.tasks.toml"), "{stderr}");
    assert!(!data.exists());
}

#[test]
fn task_ls_fails_when_manifest_is_invalid_without_partial_output_or_source_echo() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
    let malformed_toml = "version = 1\n\n[[task]]\nname = \"valid_prefix\"\nmalformed = true\n";
    fs::write(project.join("sotto.tasks.toml"), malformed_toml).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "ls"])
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "no partial output should be printed on invalid manifest"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("malformed = true"),
        "source TOML text must not be echoed in stderr"
    );
    assert!(!data.exists());
}

#[cfg(unix)]
mod interactive_approval_tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::pty::{openpty, Winsize};
    use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg};

    struct PtyRun {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    fn run_on_pty(command: &mut Command, input: &[u8]) -> PtyRun {
        let ws = Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let stdout_pty = openpty(Some(&ws), None).unwrap();
        let stderr_pty = openpty(Some(&ws), None).unwrap();
        no_echo_no_opost(&stdout_pty.slave);
        no_echo_no_opost(&stderr_pty.slave);
        command
            .stdin(Stdio::from(File::from(
                stdout_pty.slave.try_clone().unwrap(),
            )))
            .stdout(Stdio::from(File::from(
                stdout_pty.slave.try_clone().unwrap(),
            )))
            .stderr(Stdio::from(File::from(
                stderr_pty.slave.try_clone().unwrap(),
            )));
        if !input.is_empty() {
            File::from(stdout_pty.master.try_clone().unwrap())
                .write_all(input)
                .unwrap();
        }
        let mut child = command.spawn().unwrap();
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
            stdout: stdout_reader.join().unwrap(),
            stderr: stderr_reader.join().unwrap(),
        }
    }

    fn no_echo_no_opost(slave: &OwnedFd) {
        let mut termios = tcgetattr(slave).unwrap();
        termios.local_flags.remove(LocalFlags::ECHO);
        termios.local_flags.remove(LocalFlags::ISIG);
        termios.output_flags.remove(OutputFlags::OPOST);
        tcsetattr(slave, SetArg::TCSANOW, &termios).unwrap();
    }

    fn read_master(master: OwnedFd) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut file = File::from(master);
            let mut collected = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => collected.extend_from_slice(&buffer[..read]),
                    Err(error) if error.raw_os_error() == Some(Errno::EIO as i32) => break,
                    Err(error) => panic!("reading pseudo terminal: {error}"),
                }
            }
            collected
        })
    }

    fn wait_bounded(child: &mut Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("task approval did not finish within 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn project_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        let data = temp.path().join("sotto-data");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("sotto.toml"),
            "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
        )
        .unwrap();
        fs::write(
            project.join("sotto.tasks.toml"),
            "version = 1\n\n[[task]]\nname = \"test\"\ndescription = \"Run tests\"\nexecutable = \"cargo\"\nargs = [\"test\", \"--no-fail-fast\"]\nenvironments = [\"dev\", \"stage\"]\nworking_directory = \"crates/cli\"\ntimeout_seconds = 120\n",
        )
        .unwrap();
        (temp, project, data)
    }

    #[test]
    fn approval_shows_the_execution_trust_boundary_then_can_be_listed_and_revoked() {
        let (_temp, project, data) = project_fixture();
        let approved = run_on_pty(
            Command::new(env!("CARGO_BIN_EXE_sotto"))
                .args(["task", "approve", "test", "--env", "dev"])
                .current_dir(&project)
                .env("SOTTO_DATA_DIR", &data)
                .env_remove("CI")
                .env("TERM", "xterm-256color"),
            b"y\n",
        );
        assert!(
            approved.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&approved.stdout),
            String::from_utf8_lossy(&approved.stderr)
        );
        let stdout = String::from_utf8_lossy(&approved.stdout);
        assert!(stdout.contains("Project identity:"), "{stdout}");
        assert!(stdout.contains("Project root:"), "{stdout}");
        assert!(stdout.contains("Description: Run tests"), "{stdout}");
        assert!(stdout.contains("Resolved executable: "), "{stdout}");
        assert!(stdout.contains("cargo"), "{stdout}");
        assert!(stdout.contains("Argument 0: \"test\""), "{stdout}");
        assert!(
            stdout.contains("Argument 1: \"--no-fail-fast\""),
            "{stdout}"
        );
        assert!(stdout.contains("/crates/cli"), "{stdout}");
        assert!(stdout.contains("120 seconds"), "{stdout}");
        assert!(stdout.contains("not a sandbox"), "{stdout}");
        assert!(
            String::from_utf8_lossy(&approved.stderr).contains("approved task"),
            "{}",
            String::from_utf8_lossy(&approved.stderr)
        );
        assert!(!data.join("store.db").exists());

        let listed = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .args(["task", "approvals"])
            .current_dir(&project)
            .env("SOTTO_DATA_DIR", &data)
            .output()
            .unwrap();
        assert!(listed.status.success());
        assert_eq!(
            String::from_utf8(listed.stdout).unwrap(),
            "\"test\" --env \"dev\"\n"
        );

        let revoked = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .args(["task", "revoke", "test", "--env", "dev"])
            .current_dir(&project)
            .env("SOTTO_DATA_DIR", &data)
            .output()
            .unwrap();
        assert!(revoked.status.success());
        let listed = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .args(["task", "approvals"])
            .current_dir(&project)
            .env("SOTTO_DATA_DIR", &data)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(listed.stdout).unwrap(),
            "no task approvals for this project\n"
        );
    }

    #[test]
    fn declining_task_approval_does_not_create_a_grant() {
        let (_temp, project, data) = project_fixture();
        let declined = run_on_pty(
            Command::new(env!("CARGO_BIN_EXE_sotto"))
                .args(["task", "approve", "test", "--env", "dev"])
                .current_dir(&project)
                .env("SOTTO_DATA_DIR", &data)
                .env_remove("CI")
                .env("TERM", "xterm-256color"),
            b"n\n",
        );
        assert!(
            declined.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&declined.stdout),
            String::from_utf8_lossy(&declined.stderr)
        );
        assert!(!data.join("task-approvals/approvals.db").exists());
    }

    #[test]
    fn approval_data_cannot_be_written_inside_the_project_checkout() {
        let (_temp, project, _data) = project_fixture();
        let data = project.join(".sotto-data/nested");
        let denied = run_on_pty(
            Command::new(env!("CARGO_BIN_EXE_sotto"))
                .args(["task", "approve", "test", "--env", "dev"])
                .current_dir(&project)
                .env("SOTTO_DATA_DIR", &data)
                .env_remove("CI")
                .env("TERM", "xterm-256color"),
            b"y\n",
        );
        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr).contains("outside the project checkout"),
            "{}",
            String::from_utf8_lossy(&denied.stderr)
        );
        assert!(
            !project.join(".sotto-data").exists(),
            "containment rejection must happen before creating any project-local state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn approval_data_containment_resolves_existing_symlink_ancestors_before_creation() {
        use std::os::unix::fs::symlink;

        let (temp, project, _data) = project_fixture();
        let project_alias = temp.path().join("project-alias");
        symlink(&project, &project_alias).unwrap();
        let data = temp
            .path()
            .join("missing/../project-alias/.sotto-data/nested");
        let denied = run_on_pty(
            Command::new(env!("CARGO_BIN_EXE_sotto"))
                .args(["task", "approve", "test", "--env", "dev"])
                .current_dir(&project)
                .env("SOTTO_DATA_DIR", &data)
                .env_remove("CI")
                .env("TERM", "xterm-256color"),
            b"y\n",
        );

        assert!(!denied.status.success());
        assert!(
            String::from_utf8_lossy(&denied.stderr).contains("outside the project checkout"),
            "{}",
            String::from_utf8_lossy(&denied.stderr)
        );
        assert!(!project.join(".sotto-data").exists());
    }
}
