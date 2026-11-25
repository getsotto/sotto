use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use sotto_cli::agent_approvals::{project_root_key, ApprovalStore};
use tempfile::{tempdir, TempDir};

const PROJECT_ID: &str = "11111111-1111-1111-1111-111111111111";

fn project_fixture() -> (TempDir, PathBuf, PathBuf) {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("sotto.toml"),
        format!("project_id = \"{PROJECT_ID}\"\nproject = \"example\"\nenvironment = \"dev\"\n"),
    )
    .unwrap();
    (temp, project, data)
}

fn list_approvals(project: &Path, data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sotto"))
        .args(["task", "approvals"])
        .args(args)
        .current_dir(project)
        .env("SOTTO_DATA_DIR", data)
        .env("CI", "true")
        .output()
        .unwrap()
}

fn json_output(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn empty_json_approvals_do_not_print_the_human_message_or_open_the_secret_store() {
    let (_temp, project, data) = project_fixture();
    assert_eq!(
        json_output(list_approvals(&project, &data, &["--json"])),
        json!([])
    );

    let human = list_approvals(&project, &data, &[]);
    assert!(human.status.success());
    assert_eq!(human.stdout, b"no task approvals for this project\n");
    assert!(!data.join("store.db").exists());
}

#[test]
fn json_approvals_preserve_order_escaping_environment_filter_and_project_scope() {
    let (temp, project, data) = project_fixture();
    let root = project_root_key(&fs::canonicalize(&project).unwrap());
    let other_project = temp.path().join("other-project");
    fs::create_dir_all(&other_project).unwrap();
    let other_root = project_root_key(&fs::canonicalize(&other_project).unwrap());
    let approvals = ApprovalStore::open(&data).unwrap();
    let fingerprint = "a".repeat(64);
    let quoted_task = "quoted\"\\\n";
    let quoted_environment = "stage\"\\\n";
    for (task, environment) in [
        ("test", "staging"),
        (quoted_task, quoted_environment),
        ("test", "dev"),
        ("build", "production"),
    ] {
        approvals
            .approve(PROJECT_ID, &root, task, environment, &fingerprint)
            .unwrap();
    }
    approvals
        .approve("other-id", &root, "hidden-id", "dev", &fingerprint)
        .unwrap();
    approvals
        .approve(PROJECT_ID, &other_root, "hidden-root", "dev", &fingerprint)
        .unwrap();
    let stored_records = approvals.list(PROJECT_ID, &root).unwrap();

    // No task manifest or matching executable is needed to list stored records.
    assert_eq!(
        json_output(list_approvals(&project, &data, &["--json"])),
        json!([
            {"task": "build", "environment": "production"},
            {"task": quoted_task, "environment": quoted_environment},
            {"task": "test", "environment": "dev"},
            {"task": "test", "environment": "staging"},
        ])
    );
    assert_eq!(
        json_output(list_approvals(
            &project,
            &data,
            &["--json", "--env", "staging"]
        )),
        json!([{"task": "test", "environment": "staging"}])
    );
    assert_eq!(
        json_output(list_approvals(
            &project,
            &data,
            &["--json", "--env", "missing"]
        )),
        json!([])
    );
    let nested = project.join("nested");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        json_output(list_approvals(
            &nested,
            &data,
            &["--json", "--env", "staging"]
        )),
        json!([{"task": "test", "environment": "staging"}])
    );
    assert_eq!(approvals.list(PROJECT_ID, &root).unwrap(), stored_records);
    assert!(!data.join("store.db").exists());
}

#[test]
fn json_approvals_reject_an_explicitly_empty_environment_without_creating_data() {
    let (_temp, project, data) = project_fixture();
    let output = list_approvals(&project, &data, &["--json", "--env", ""]);
    let human = list_approvals(&project, &data, &["--env", ""]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.status, human.status);
    assert_eq!(output.stderr, human.stderr);
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("environment filter must not be empty")
    );
    assert!(!data.exists());
}

#[test]
fn json_approvals_propagate_storage_failures_instead_of_returning_an_empty_array() {
    let (_temp, project, data) = project_fixture();
    fs::write(&data, b"obstructing file").unwrap();
    let output = list_approvals(&project, &data, &["--json"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("creating"));
    assert_eq!(fs::read(&data).unwrap(), b"obstructing file");
}
