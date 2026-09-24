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
        assert!(!output.status.success());
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
        assert!(!output.status.success());
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
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("missing required argument <NAME>"),
        "env use without arg stderr: {stderr}"
    );
}
