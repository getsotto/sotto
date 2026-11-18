use std::process::Command;

#[test]
fn invalid_audit_limits_fail_before_local_setup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    for (label, option) in [("zero", "--limit=0"), ("negative", "--limit=-1")] {
        let data_dir = scratch.path().join(label);
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .env_remove("SOTTO_TOKEN")
            .env_remove("SOTTO_PASSWORD")
            .args(["org", "audit", "example-org", option])
            .output()
            .expect("run sotto");

        assert_eq!(output.status.code(), Some(2), "accepted {option}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(stderr.contains("--limit"), "{option}: {stderr}");
        assert!(stderr.contains("1.."), "{option}: {stderr}");
        assert!(!stderr.contains("not logged in"), "{option}: {stderr}");
        assert!(
            !data_dir.exists(),
            "{option} created {}",
            data_dir.display()
        );
    }
}

#[test]
fn audit_help_explains_positive_limit_and_server_cap() {
    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["org", "audit", "--help"])
        .output()
        .expect("run sotto help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("positive"), "{stdout}");
    assert!(stdout.contains("server caps at 500"), "{stdout}");
}
