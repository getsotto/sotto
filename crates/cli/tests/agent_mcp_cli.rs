use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use tempfile::tempdir;

fn write_project(root: &std::path::Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("sotto.toml"),
        "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
    )
    .unwrap();
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}).to_string()
}

fn run_stdio_mcp(
    project: &std::path::Path,
    data_dir: &std::path::Path,
    input: &str,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .arg("mcp")
        .current_dir(project)
        .env("SOTTO_DATA_DIR", data_dir)
        .env("SOTTO_TOKEN", "parent-token-canary")
        .env("SOTTO_PASSWORD", "parent-password-canary")
        .env("SOTTO_SECRET_KEY", "parent-key-canary")
        .env_remove("CI")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn mcp_requires_a_project_before_initialising_sotto_state() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("outside-project");
    let data = temp.path().join("sotto-data");
    fs::create_dir_all(&project).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .arg("mcp")
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("run `sotto init`"), "{stderr}");
    assert!(
        !data.exists(),
        "MCP startup should validate the project first"
    );
}

#[test]
fn mcp_requires_a_valid_task_file_before_initialising_sotto_state() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    write_project(&project);

    let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
        .arg("mcp")
        .current_dir(&project)
        .env("SOTTO_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("sotto.tasks.toml"), "{stderr}");
    assert!(
        !data.exists(),
        "MCP startup should validate the task file first"
    );
}

#[cfg(unix)]
#[test]
fn stdio_mcp_lists_only_strict_task_tools_and_never_spawns_without_approval() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let data = temp.path().join("sotto-data");
    write_project(&project);
    let marker = project.join("spawned");
    let manifest = format!(
        "version = 1\n\n[[task]]\nname = \"danger\"\ndescription = \"A test-only task\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n",
        format!("touch {:?}", marker)
    );
    fs::write(project.join("sotto.tasks.toml"), manifest).unwrap();

    let mut input = vec![
        request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"sotto-mcp-test","version":"1"}
            }),
        ),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
        request(2, "tools/list", json!({})),
        request(3, "tools/call", json!({"name":"list_tasks","arguments":{}})),
        request(
            4,
            "tools/call",
            json!({"name":"run_task","arguments":{"task":"danger","environment":"dev","command":"touch /tmp/forbidden"}}),
        ),
        request(
            5,
            "tools/call",
            json!({"name":"run_task","arguments":{"task":"danger","environment":"dev"}}),
        ),
        request(
            6,
            "tools/call",
            json!({"name":"run_task","arguments":{"task":"danger","environment":"prod"}}),
        ),
    ];
    input.push(String::new());
    let output = run_stdio_mcp(&project, &data, &input.join("\n"));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let responses = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap_or_else(|error| {
                panic!("stdout must contain only MCP JSON messages, got {line:?}: {error}")
            })
        })
        .collect::<Vec<_>>();
    let by_id = |id| {
        responses
            .iter()
            .find(|response| response["id"] == id)
            .unwrap()
    };

    let listed_tools = by_id(2)["result"]["tools"].as_array().unwrap();
    let mut names = listed_tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["list_tasks", "run_task"]);
    let run_schema = listed_tools
        .iter()
        .find(|tool| tool["name"] == "run_task")
        .unwrap();
    assert_eq!(run_schema["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        run_schema["inputSchema"]["required"],
        json!(["task", "environment"])
    );
    assert_eq!(run_schema["outputSchema"]["additionalProperties"], false);
    assert_eq!(
        run_schema["outputSchema"]["required"],
        json!(["task", "environment", "status", "exit_code", "summary"])
    );

    let listed_tasks = &by_id(3)["result"]["structuredContent"]["tasks"];
    assert_eq!(listed_tasks[0]["name"], "danger");
    assert_eq!(listed_tasks[0]["description"], "A test-only task");
    assert_eq!(listed_tasks[0]["environments"], json!(["dev"]));
    assert!(listed_tasks.to_string().find("approval").is_none());

    assert!(
        by_id(4).get("error").is_some(),
        "extra command input must be rejected"
    );
    assert_eq!(by_id(5)["result"]["isError"], true);
    assert!(by_id(5)["result"]["structuredContent"]["summary"]
        .as_str()
        .unwrap()
        .contains("no current local approval"));
    assert_eq!(by_id(6)["result"]["isError"], true);
    assert!(by_id(6)["result"]["structuredContent"]["summary"]
        .as_str()
        .unwrap()
        .contains("does not allow environment"));
    assert!(!marker.exists(), "an unapproved task must not spawn");
    assert!(!stdout.contains("parent-token-canary"));
    assert!(!stdout.contains("parent-password-canary"));
    assert!(!stdout.contains("parent-key-canary"));
}
