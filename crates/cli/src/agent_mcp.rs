//! Local MCP stdio adapter for approved project tasks.
//!
//! This module exposes task metadata and the shared approved-task runner. It never reads or
//! returns secret values, creates approvals, or returns child output.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RunningService, Service};
use rmcp::RoleServer;
use rmcp::{transport::stdio, ServiceExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::agent_approvals::ApprovalStore;
use crate::agent_runner::{run_approved_task, TaskRunOutcome, TaskRunStatus};
use crate::agent_tasks::TaskManifest;
use crate::commands::App;
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::store::Store;

const LIST_TASKS: &str = "list_tasks";
const RUN_TASK: &str = "run_task";

/// MCP server state. SQLite and keychain access is held behind a short synchronous lock because
/// the runner performs all authorization/decryption before returning its owned async future.
struct ServerState {
    project_root: PathBuf,
    store: Store,
    keychain: Box<dyn Keychain + Send>,
    approvals: ApprovalStore,
}

/// The local Sotto task server.
#[derive(Clone)]
pub struct SottoMcpServer {
    state: Arc<Mutex<ServerState>>,
}

impl SottoMcpServer {
    pub fn new<K>(
        project_root: PathBuf,
        store: Store,
        keychain: K,
        approvals: ApprovalStore,
    ) -> Self
    where
        K: Keychain + Send + 'static,
    {
        Self {
            state: Arc::new(Mutex::new(ServerState {
                project_root,
                store,
                keychain: Box::new(keychain),
                approvals,
            })),
        }
    }

    fn list_tasks(&self) -> Result<Value> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Input("Sotto MCP state is unavailable".into()))?;
        let manifest = TaskManifest::load_from(&state.project_root)?;
        #[derive(Serialize)]
        struct ListedTask<'a> {
            name: &'a str,
            description: &'a str,
            environments: &'a [String],
        }
        let tasks = manifest
            .tasks()
            .iter()
            .map(|task| ListedTask {
                name: task.name(),
                description: task.description(),
                environments: task.environments(),
            })
            .collect::<Vec<_>>();
        Ok(json!({ "tasks": tasks }))
    }

    fn prepare_run(
        &self,
        task: &str,
        environment: &str,
        cancellation: watch::Receiver<bool>,
    ) -> Result<impl Future<Output = Result<TaskRunOutcome>> + Send + 'static> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Input("Sotto MCP state is unavailable".into()))?;
        let app = App::new(&state.store, state.keychain.as_ref());
        run_approved_task(
            &app,
            &state.approvals,
            &state.project_root,
            task,
            environment,
            cancellation,
        )
    }

    async fn execute_task<C>(
        &self,
        task: &str,
        environment: &str,
        request_cancellation: C,
    ) -> CallToolResult
    where
        C: Future<Output = ()> + Send,
    {
        let (cancel, receiver) = watch::channel(false);
        let run = match self.prepare_run(task, environment, receiver) {
            Ok(run) => run,
            Err(error) => return run_error(task, environment, safe_error_message(&error)),
        };
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => match result {
                Ok(outcome) => outcome_result(outcome),
                Err(error) => run_error(task, environment, safe_error_message(&error)),
            },
            _ = request_cancellation => {
                let _ = cancel.send(true);
                match run.await {
                    Ok(outcome) => outcome_result(outcome),
                    Err(error) => run_error(task, environment, safe_error_message(&error)),
                }
            }
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            LIST_TASKS => {
                if !arguments.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "list_tasks accepts no arguments",
                        None,
                    ));
                }
                match self.list_tasks() {
                    Ok(tasks) => Ok(CallToolResult::structured(tasks).into()),
                    Err(error) => Ok(tool_error(safe_error_message(&error)).into()),
                }
            }
            RUN_TASK => {
                let (task, environment) = parse_run_arguments(&arguments)?;
                Ok(self
                    .execute_task(&task, &environment, context.ct.cancelled())
                    .await
                    .into())
            }
            _ => Err(ErrorData::invalid_params("unknown Sotto tool", None)),
        }
    }
}

impl ServerHandler for SottoMcpServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Sotto exposes declared project tasks. Running a task requires local approval and a separately unlocked Sotto session. Approved project code receives plaintext secrets from the selected environment and is trusted to use them safely.",
            );
        info.server_info = Implementation::new("sotto", env!("CARGO_PKG_VERSION"));
        info
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tool_definitions())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_definitions()
            .into_iter()
            .find(|tool| tool.name.as_ref() == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        SottoMcpServer::call_tool(self, request, context).await
    }
}

/// Run the stdio service until the MCP client closes its input or the server is stopped.
pub async fn serve_stdio(server: SottoMcpServer) -> Result<()> {
    let service = server
        .serve(stdio())
        .await
        .map_err(|error| Error::Io(format!("starting Sotto MCP server: {error}")))?;
    wait_for_shutdown(service, tokio::signal::ctrl_c()).await
}

async fn wait_for_shutdown<S, F>(service: RunningService<RoleServer, S>, shutdown: F) -> Result<()>
where
    S: Service<RoleServer>,
    F: Future<Output = std::io::Result<()>> + Send,
{
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);

    tokio::select! {
        result = &mut waiting => {
            result.map_err(|error| Error::Io(format!("Sotto MCP server stopped unexpectedly: {error}")))?;
            Ok(())
        }
        signal = shutdown => {
            // Cancelling the RMCP service cancels active request contexts. The run_task handler
            // forwards that cancellation to the shared runner and waits for its child to stop.
            cancellation.cancel();
            (&mut waiting)
                .await
                .map_err(|error| Error::Io(format!("stopping Sotto MCP server: {error}")))?;
            signal.map_err(|error| Error::Io(format!("waiting for Sotto MCP shutdown signal: {error}")))?;
            Ok(())
        }
    }
}

fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            LIST_TASKS,
            "List project-declared task names, descriptions, and allowed environments. Does not expose approvals or secret metadata.",
            schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
        ),
        Tool::new(
            RUN_TASK,
            "Run a declared task with one allowed Sotto environment after local approval. The task receives that environment's plaintext secrets. Returns task, environment, status, exit_code, and a Sotto-generated summary. Child output is discarded.",
            schema(json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 64,
                        "pattern": "^[a-z0-9_-]+$"
                    },
                    "environment": { "type": "string", "minLength": 1, "maxLength": 4096 }
                },
                "required": ["task", "environment"],
                "additionalProperties": false
            })),
        )
        .with_raw_output_schema(schema(json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" },
                "environment": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["completed", "failed", "timed_out", "cancelled", "could_not_start", "wait_failed", "error"]
                },
                "exit_code": { "type": ["integer", "null"] },
                "summary": { "type": "string" }
            },
            "required": ["task", "environment", "status", "exit_code", "summary"],
            "additionalProperties": false
        }))),
    ]
}

fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().expect("tool schema is an object").clone())
}

fn parse_run_arguments(arguments: &JsonObject) -> std::result::Result<(String, String), ErrorData> {
    if arguments.len() != 2 {
        return Err(ErrorData::invalid_params(
            "run_task requires exactly task and environment",
            None,
        ));
    }
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .filter(|value| valid_task_name(value))
        .ok_or_else(|| {
            ErrorData::invalid_params("task must be a valid declared task name", None)
        })?;
    let environment = arguments
        .get("environment")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 4096 && !value.contains('\0'))
        .ok_or_else(|| {
            ErrorData::invalid_params("environment must be a valid environment name", None)
        })?;
    Ok((task.to_owned(), environment.to_owned()))
}

fn valid_task_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && value.is_ascii()
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
        })
}

fn outcome_result(outcome: TaskRunOutcome) -> CallToolResult {
    let value = json!({
        "task": outcome.task_name,
        "environment": outcome.environment,
        "status": status_name(outcome.status),
        "exit_code": outcome.exit_code,
        "summary": outcome.summary(),
    });
    if outcome.status == TaskRunStatus::Completed {
        CallToolResult::structured(value)
    } else {
        CallToolResult::structured_error(value)
    }
}

fn status_name(status: TaskRunStatus) -> &'static str {
    match status {
        TaskRunStatus::Completed => "completed",
        TaskRunStatus::Failed => "failed",
        TaskRunStatus::TimedOut => "timed_out",
        TaskRunStatus::Cancelled => "cancelled",
        TaskRunStatus::CouldNotStart => "could_not_start",
        TaskRunStatus::WaitFailed => "wait_failed",
    }
}

fn safe_error_message(error: &Error) -> String {
    match error {
        Error::Locked => {
            "Sotto is locked or the session expired. Run `sotto unlock` in a separate terminal, then retry.".into()
        }
        Error::NoIdentity => "No Sotto identity is available. Run `sotto init` in a Sotto project first.".into(),
        Error::Input(_) | Error::Config(_) | Error::NotFound(_) | Error::NoConfig(_) => {
            error.to_string()
        }
        _ => "Sotto could not complete the task request. Check local Sotto state and project configuration.".into(),
    }
}

fn tool_error(message: String) -> CallToolResult {
    CallToolResult::structured_error(json!({ "status": "error", "summary": message }))
}

fn run_error(task: &str, environment: &str, message: String) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "task": task,
        "environment": environment,
        "status": "error",
        "exit_code": null,
        "summary": message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_approvals::{approval_fingerprint, project_root_key};
    use crate::agent_tasks::TaskManifest;
    use crate::keychain::MemoryKeychain;
    use crate::session;
    use crate::vault::Vault;
    use std::time::Duration;

    #[test]
    fn tools_have_strict_schemas_and_only_the_agreed_names() {
        let mut tools = tool_definitions();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            [LIST_TASKS, RUN_TASK]
        );
        for tool in tools {
            assert_eq!(tool.input_schema["additionalProperties"], false);
        }
        let run_task = tool_definitions()
            .into_iter()
            .find(|tool| tool.name.as_ref() == RUN_TASK)
            .unwrap();
        assert_eq!(
            run_task.input_schema["required"],
            json!(["task", "environment"])
        );
        assert_eq!(
            run_task.input_schema["properties"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        let output_schema = run_task.output_schema.unwrap();
        assert_eq!(output_schema["additionalProperties"], false);
        assert_eq!(
            output_schema["required"],
            json!(["task", "environment", "status", "exit_code", "summary"])
        );
    }

    #[test]
    fn run_task_arguments_reject_extra_fields_and_wrong_types() {
        assert!(parse_run_arguments(
            json!({"task":"test","environment":"dev"})
                .as_object()
                .unwrap()
        )
        .is_ok());
        assert!(parse_run_arguments(
            json!({"task":"test","environment":"dev","args":[]})
                .as_object()
                .unwrap()
        )
        .is_err());
        assert!(
            parse_run_arguments(json!({"task":"test","environment":1}).as_object().unwrap())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn locked_task_run_returns_separate_unlock_guidance_without_spawning() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(
            &store,
            &keychain,
            b"test password",
            Duration::from_secs(3600),
        )
        .unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "MCP test").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "MCP test".into(),
            environment: "dev".into(),
            org_id: None,
        };
        config.save_to(root.path()).unwrap();
        let marker = root.path().join("spawned");
        let manifest = format!(
            "version = 1\n\n[[task]]\nname = \"verify\"\ndescription = \"Test task\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n",
            format!("touch {:?}", marker)
        );
        std::fs::write(root.path().join("sotto.tasks.toml"), manifest).unwrap();
        let approvals = ApprovalStore::open(data.path()).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let manifest = TaskManifest::load_from(&canonical_root).unwrap();
        let task = &manifest.tasks()[0];
        let executable = task.resolve_executable(&canonical_root).unwrap();
        let fingerprint =
            approval_fingerprint(&manifest.fingerprint_for(task.name()).unwrap(), &executable)
                .unwrap();
        approvals
            .approve(
                &config.project_id,
                &project_root_key(&canonical_root),
                task.name(),
                "dev",
                &fingerprint,
            )
            .unwrap();
        let server = SottoMcpServer::new(canonical_root, store, keychain, approvals);
        let state = server.state.clone();
        session::lock(state.lock().unwrap().keychain.as_ref()).unwrap();
        let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let mut client = ().serve(client_transport).await.unwrap();
        let locked = client
            .call_tool(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"verify", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await;
        let locked = locked.unwrap();
        assert_eq!(locked.is_error, Some(true));
        assert_eq!(
            locked.structured_content.as_ref().unwrap()["task"],
            "verify"
        );
        assert_eq!(
            locked.structured_content.as_ref().unwrap()["environment"],
            "dev"
        );
        assert!(locked.structured_content.as_ref().unwrap()["summary"]
            .as_str()
            .unwrap()
            .contains("sotto unlock"));
        assert!(!marker.exists());

        {
            let state = state.lock().unwrap();
            session::unlock(
                &state.store,
                state.keychain.as_ref(),
                b"test password",
                Duration::from_millis(1),
            )
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
        let expired = client
            .call_tool(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"verify", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await;
        let expired = expired.unwrap();
        assert_eq!(expired.is_error, Some(true));
        assert!(expired.structured_content.as_ref().unwrap()["summary"]
            .as_str()
            .unwrap()
            .contains("sotto unlock"));
        assert!(!marker.exists());
        client.close().await.unwrap();
        server_task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approved_mcp_run_injects_canary_and_returns_no_child_output() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(
            &store,
            &keychain,
            b"test password",
            Duration::from_secs(3600),
        )
        .unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "MCP canary test").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "MCP canary test".into(),
            environment: "dev".into(),
            org_id: None,
        };
        config.save_to(root.path()).unwrap();
        let marker = root.path().join("canary-seen");
        let script = format!(
            r#"test "$MCP_CANARY" = "synthetic-value" && touch {:?}; printf 'synthetic-value from child output'"#,
            marker
        );
        let manifest = format!(
            "version = 1\n\n[[task]]\nname = \"verify\"\ndescription = \"Canary test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n\n[[task]]\nname = \"fail\"\ndescription = \"Failure mapping test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", \"printf 'hidden failure output'; exit 7\"]\nenvironments = [\"dev\"]\n\n[[task]]\nname = \"slow\"\ndescription = \"Timeout mapping test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", \"sleep 5\"]\nenvironments = [\"dev\"]\ntimeout_seconds = 1\n",
            script
        );
        std::fs::write(root.path().join("sotto.tasks.toml"), manifest).unwrap();
        let app = App::new(&store, &keychain);
        app.set(&config, "MCP_CANARY", b"synthetic-value").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let manifest = TaskManifest::load_from(&canonical_root).unwrap();
        let approvals = ApprovalStore::open(data.path()).unwrap();
        for task in manifest.tasks() {
            let executable = task.resolve_executable(&canonical_root).unwrap();
            let fingerprint =
                approval_fingerprint(&manifest.fingerprint_for(task.name()).unwrap(), &executable)
                    .unwrap();
            approvals
                .approve(
                    &config.project_id,
                    &project_root_key(&canonical_root),
                    task.name(),
                    "dev",
                    &fingerprint,
                )
                .unwrap();
        }
        let server = SottoMcpServer::new(canonical_root, store, keychain, approvals);
        let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let mut client = ().serve(client_transport).await.unwrap();
        assert_eq!(client.list_tools(None).await.unwrap().tools.len(), 2);

        let result = client
            .call_tool(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"verify", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert!(
            marker.exists(),
            "the approved task should receive its canary"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("synthetic-value"), "{serialized}");
        assert!(!serialized.contains("child output"), "{serialized}");

        let failed = client
            .call_tool(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"fail", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            failed.structured_content.as_ref().unwrap()["status"],
            "failed"
        );
        assert_eq!(failed.structured_content.as_ref().unwrap()["exit_code"], 7);
        assert!(!serde_json::to_string(&failed)
            .unwrap()
            .contains("hidden failure output"));

        let timed_out = client
            .call_tool(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"slow", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(timed_out.is_error, Some(true));
        assert_eq!(
            timed_out.structured_content.as_ref().unwrap()["status"],
            "timed_out"
        );
        assert_eq!(
            timed_out.structured_content.as_ref().unwrap()["exit_code"],
            Value::Null
        );

        client.close().await.unwrap();
        server_task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_cancellation_and_server_shutdown_stop_approved_children() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(
            &store,
            &keychain,
            b"test password",
            Duration::from_secs(3600),
        )
        .unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "MCP cancellation test").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "MCP cancellation test".into(),
            environment: "dev".into(),
            org_id: None,
        };
        config.save_to(root.path()).unwrap();
        let started = root.path().join("started");
        let marker = root.path().join("should-not-finish");
        let shutdown_started = root.path().join("shutdown-started");
        let shutdown_marker = root.path().join("shutdown-should-not-finish");
        let manifest = format!(
            "version = 1\n\n[[task]]\nname = \"slow\"\ndescription = \"Cancellation test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n\n[[task]]\nname = \"shutdown_slow\"\ndescription = \"Shutdown test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n",
            format!("touch {:?}; sleep 1; touch {:?}", started, marker),
            format!(
                "touch {:?}; sleep 1; touch {:?}",
                shutdown_started, shutdown_marker
            ),
        );
        std::fs::write(root.path().join("sotto.tasks.toml"), manifest).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let manifest = TaskManifest::load_from(&canonical_root).unwrap();
        let approvals = ApprovalStore::open(data.path()).unwrap();
        for task in manifest.tasks() {
            let executable = task.resolve_executable(&canonical_root).unwrap();
            let fingerprint =
                approval_fingerprint(&manifest.fingerprint_for(task.name()).unwrap(), &executable)
                    .unwrap();
            approvals
                .approve(
                    &config.project_id,
                    &project_root_key(&canonical_root),
                    task.name(),
                    "dev",
                    &fingerprint,
                )
                .unwrap();
        }
        let server = SottoMcpServer::new(canonical_root, store, keychain, approvals);
        let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
        let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(async move {
            let service = server.serve(server_transport).await.unwrap();
            wait_for_shutdown(service, async move {
                let _ = shutdown_signal.await;
                Ok(())
            })
            .await
            .unwrap();
        });
        let mut client = ().serve(client_transport).await.unwrap();
        let arguments = json!({"task":"slow", "environment":"dev"})
            .as_object()
            .unwrap()
            .clone();
        let request =
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new(RUN_TASK).with_arguments(arguments),
            ));
        let handle = client
            .peer()
            .send_cancellable_request(request, rmcp::service::PeerRequestOptions::no_options())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task child should start before cancellation");
        handle
            .cancel(Some("test cancellation".into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect_err("cancelled task must not complete its post-sleep action");
        assert!(!marker.exists());

        let shutdown_request =
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new(RUN_TASK).with_arguments(
                    json!({"task":"shutdown_slow", "environment":"dev"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ));
        let _active_run = client
            .peer()
            .send_cancellable_request(
                shutdown_request,
                rmcp::service::PeerRequestOptions::no_options(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !shutdown_started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shutdown task child should start before server shutdown");
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server shutdown should wait for its active child to stop")
            .unwrap();
        assert!(!shutdown_marker.exists());
        client.close().await.unwrap();
    }
}
