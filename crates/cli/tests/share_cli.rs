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
