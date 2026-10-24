//! Integration tests for `sotto org audit` argument validation.

use std::process::Command;

#[test]
fn org_audit_non_positive_limit_fails_before_state_or_login() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let data_dir = scratch.path().join("sotto-data");

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", &data_dir)
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_PASSWORD")
        .args(["org", "audit", "example-org", "--limit", "0"])
        .output()
        .expect("run sotto org audit");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("error: invalid value '0' for '--limit <LIMIT>'"),
        "expected error message on stderr: {stderr}"
    );
    assert!(
        !data_dir.exists(),
        "invalid limit must fail before creating local state or data dir"
    );

    let output_neg = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .current_dir(scratch.path())
        .env("SOTTO_DATA_DIR", &data_dir)
        .env_remove("SOTTO_TOKEN")
        .env_remove("SOTTO_PASSWORD")
        .args(["org", "audit", "example-org", "--limit=-1"])
        .output()
        .expect("run sotto org audit");

    assert_eq!(output_neg.status.code(), Some(2));
    let stderr_neg = String::from_utf8(output_neg.stderr).expect("UTF-8 stderr");
    assert!(
        stderr_neg.contains("error: invalid value '-1' for '--limit <LIMIT>'"),
        "expected error message on stderr: {stderr_neg}"
    );
    assert!(
        !data_dir.exists(),
        "invalid limit must fail before creating local state or data dir"
    );
}
