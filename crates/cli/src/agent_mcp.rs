//! Local MCP stdio adapter for approved project tasks.
//!
//! This module exposes task metadata and the shared approved-task runner. It never reads or
//! returns secret values, creates approvals, or returns child output.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::task::JoinSet;

use crate::agent_approvals::ApprovalStore;
use crate::agent_runner::{run_approved_task, TaskRunOutcome, TaskRunStatus};
use crate::agent_tasks::TaskManifest;
use crate::commands::App;
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::store::Store;

const LIST_TASKS: &str = "list_tasks";
const RUN_TASK: &str = "run_task";

const INSTRUCTIONS: &str = "Sotto exposes declared project tasks. Running a task requires local approval and a separately unlocked Sotto session. Approved project code receives plaintext secrets from the selected environment and is trusted to use them safely.";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";
/// Answer a supported request verbatim; otherwise answer the newest version
/// this adapter implements so the client can decide whether to proceed.
fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    match requested {
        Some("2024-11-05") => "2024-11-05",
        Some("2025-03-26") => "2025-03-26",
        Some("2025-06-18") => "2025-06-18",
        _ => DEFAULT_PROTOCOL_VERSION,
    }
}
const MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

const CODE_PARSE_ERROR: i64 = -32700;
const CODE_INVALID_REQUEST: i64 = -32600;
const CODE_METHOD_NOT_FOUND: i64 = -32601;
const CODE_INVALID_PARAMS: i64 = -32602;
const CODE_NOT_INITIALIZED: i64 = -32002;

/// A tool result in the wire shape MCP clients already consume from this server.
struct ToolResult {
    structured: Value,
    is_error: bool,
}

impl ToolResult {
    fn wire(&self) -> Value {
        json!({
            "content": [{"type": "text", "text": self.structured.to_string()}],
            "structuredContent": self.structured,
            "isError": self.is_error,
        })
    }
}

/// A JSON-RPC protocol error, as opposed to a tool execution error result.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: CODE_INVALID_PARAMS,
            message: message.into(),
        }
    }
}

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

    async fn execute_task(
        &self,
        task: &str,
        environment: &str,
        cancelled: watch::Receiver<bool>,
    ) -> ToolResult {
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
            () = wait_cancelled(cancelled) => {
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
        name: &str,
        arguments: &Map<String, Value>,
        cancelled: watch::Receiver<bool>,
    ) -> std::result::Result<ToolResult, RpcError> {
        match name {
            LIST_TASKS => {
                if !arguments.is_empty() {
                    return Err(RpcError::invalid_params("list_tasks accepts no arguments"));
                }
                match self.list_tasks() {
                    Ok(tasks) => Ok(ToolResult {
                        structured: tasks,
                        is_error: false,
                    }),
                    Err(error) => Ok(tool_error(safe_error_message(&error))),
                }
            }
            RUN_TASK => {
                let (task, environment) = parse_run_arguments(arguments)?;
                Ok(self.execute_task(&task, &environment, cancelled).await)
            }
            _ => Err(RpcError::invalid_params("unknown Sotto tool")),
        }
    }

    /// Serve newline-delimited MCP JSON-RPC over a split transport until the
    /// reader closes or `shutdown` resolves. In-flight runs are cancelled and
    /// awaited before returning, so no approved child outlives the server.
    async fn serve_transport<R, W, F>(self, reader: R, writer: W, shutdown: F) -> Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        F: Future<Output = ()> + Send,
    {
        let writer = Arc::new(AsyncMutex::new(tokio::io::BufWriter::new(writer)));
        let inflight: Arc<Mutex<HashMap<String, watch::Sender<bool>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut lines = BufReader::new(reader).lines();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut initialized = false;
        let mut outcome: Result<()> = Ok(());
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            if line.len() > MAX_MESSAGE_BYTES {
                                outcome = Err(Error::Io("MCP message exceeds size limit".into()));
                                break;
                            }
                            if line.trim().is_empty() {
                                continue;
                            }
                            if let Err(error) = self
                                .handle_message(&line, &writer, &inflight, &mut tasks, &mut initialized)
                                .await
                            {
                                outcome = Err(error);
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            outcome = Err(Error::Io(format!("reading MCP request: {error}")));
                            break;
                        }
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }

        // Every exit path cancels active runs and waits for their children to
        // stop; a cancelled request gets no response.
        if let Ok(mut inflight) = inflight.lock() {
            for (_, cancel) in inflight.drain() {
                let _ = cancel.send(true);
            }
        }
        while tasks.join_next().await.is_some() {}
        let mut writer = writer.lock().await;
        let flushed = writer
            .flush()
            .await
            .map_err(|error| Error::Io(format!("flushing MCP response: {error}")));
        outcome.and(flushed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_message<W>(
        &self,
        line: &str,
        writer: &Arc<AsyncMutex<tokio::io::BufWriter<W>>>,
        inflight: &Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
        tasks: &mut JoinSet<()>,
        initialized: &mut bool,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => {
                return write_message(
                    writer,
                    &failure(Value::Null, CODE_PARSE_ERROR, "parse error"),
                )
                .await;
            }
        };
        let Some(object) = message.as_object() else {
            return write_message(
                writer,
                &failure(Value::Null, CODE_INVALID_REQUEST, "invalid request"),
            )
            .await;
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let id = object.get("id").cloned().unwrap_or(Value::Null);
            return write_message(
                writer,
                &failure(id, CODE_INVALID_REQUEST, "invalid request"),
            )
            .await;
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // Responses to requests this server never sends need no handling.
            return Ok(());
        };
        let method = method.to_owned();

        // Notifications carry no id and never get a response.
        let Some(id) = object.get("id").cloned() else {
            if method == "notifications/cancelled" {
                if let Some(request_id) = object
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                {
                    let key = request_id.to_string();
                    if let Ok(inflight) = inflight.lock() {
                        if let Some(cancel) = inflight.get(&key) {
                            let _ = cancel.send(true);
                        }
                    }
                }
            }
            return Ok(());
        };

        if method != "initialize" && !*initialized {
            return write_message(
                writer,
                &failure(id, CODE_NOT_INITIALIZED, "server not initialized"),
            )
            .await;
        }

        match method.as_str() {
            "initialize" => {
                *initialized = true;
                let version = negotiate_protocol_version(
                    object
                        .get("params")
                        .and_then(|params| params.get("protocolVersion"))
                        .and_then(Value::as_str),
                );
                let result = json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "sotto", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                });
                write_message(writer, &success(id, result)).await
            }
            "ping" => write_message(writer, &success(id, json!({}))).await,
            "tools/list" => {
                write_message(writer, &success(id, json!({"tools": tool_definitions()}))).await
            }
            "tools/call" => {
                let params = object.get("params").and_then(Value::as_object);
                let name = params
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = match params.and_then(|params| params.get("arguments")) {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(arguments)) => arguments.clone(),
                    Some(_) => {
                        return write_message(
                            writer,
                            &failure(id, CODE_INVALID_PARAMS, "tool arguments must be an object"),
                        )
                        .await;
                    }
                };
                let (cancel, cancelled) = watch::channel(false);
                let key = id.to_string();
                if let Ok(mut inflight) = inflight.lock() {
                    inflight.insert(key.clone(), cancel);
                }
                let server = self.clone();
                let writer = Arc::clone(writer);
                let inflight = Arc::clone(inflight);
                tasks.spawn(async move {
                    let result = server.call_tool(&name, &arguments, cancelled.clone()).await;
                    if let Ok(mut inflight) = inflight.lock() {
                        inflight.remove(&key);
                    }
                    if *cancelled.borrow() {
                        return;
                    }
                    let response = match result {
                        Ok(result) => success(id, result.wire()),
                        Err(error) => failure(id, error.code, &error.message),
                    };
                    let _ = write_message(&writer, &response).await;
                });
                Ok(())
            }
            _ => {
                write_message(
                    writer,
                    &failure(id, CODE_METHOD_NOT_FOUND, "method not found"),
                )
                .await
            }
        }
    }
}

async fn wait_cancelled(mut cancelled: watch::Receiver<bool>) {
    loop {
        if *cancelled.borrow() {
            return;
        }
        if cancelled.changed().await.is_err() {
            return;
        }
    }
}

fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn write_message<W>(
    writer: &Arc<AsyncMutex<tokio::io::BufWriter<W>>>,
    message: &Value,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    let mut writer = writer.lock().await;
    writer
        .write_all(message.to_string().as_bytes())
        .await
        .map_err(|error| Error::Io(format!("writing MCP response: {error}")))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|error| Error::Io(format!("writing MCP response: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| Error::Io(format!("writing MCP response: {error}")))?;
    Ok(())
}

/// Run the stdio service until the MCP client closes its input or the server is stopped.
pub async fn serve_stdio(server: SottoMcpServer) -> Result<()> {
    server
        .serve_transport(tokio::io::stdin(), tokio::io::stdout(), async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": LIST_TASKS,
            "description": "List project-declared task names, descriptions, and allowed environments. Does not expose approvals or secret metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
        }),
        json!({
            "name": RUN_TASK,
            "description": "Run a declared task with one allowed Sotto environment after local approval. The task receives that environment's plaintext secrets. Returns task, environment, status, exit_code, and a Sotto-generated summary. Child output is discarded.",
            "inputSchema": {
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
            },
            "outputSchema": {
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
            },
        }),
    ]
}

fn parse_run_arguments(
    arguments: &Map<String, Value>,
) -> std::result::Result<(String, String), RpcError> {
    if arguments.len() != 2 {
        return Err(RpcError::invalid_params(
            "run_task requires exactly task and environment",
        ));
    }
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .filter(|value| valid_task_name(value))
        .ok_or_else(|| RpcError::invalid_params("task must be a valid declared task name"))?;
    let environment = arguments
        .get("environment")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 4096 && !value.contains('\0'))
        .ok_or_else(|| RpcError::invalid_params("environment must be a valid environment name"))?;
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

fn outcome_result(outcome: TaskRunOutcome) -> ToolResult {
    let value = json!({
        "task": outcome.task_name,
        "environment": outcome.environment,
        "status": status_name(outcome.status),
        "exit_code": outcome.exit_code,
        "summary": outcome.summary(),
    });
    ToolResult {
        is_error: outcome.status != TaskRunStatus::Completed,
        structured: value,
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

fn tool_error(message: String) -> ToolResult {
    ToolResult {
        structured: json!({ "status": "error", "summary": message }),
        is_error: true,
    }
}

fn run_error(task: &str, environment: &str, message: String) -> ToolResult {
    ToolResult {
        structured: json!({
            "task": task,
            "environment": environment,
            "status": "error",
            "exit_code": null,
            "summary": message,
        }),
        is_error: true,
    }
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

    /// Minimal JSON-RPC client replacing the SDK client these tests used
    /// before; it speaks the same newline-delimited wire protocol.
    struct TestClient {
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        next_id: u64,
    }

    impl TestClient {
        async fn send(&mut self, message: Value) {
            self.writer
                .write_all(message.to_string().as_bytes())
                .await
                .unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn send_request(&mut self, method: &str, params: Value) -> u64 {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            id
        }

        async fn read_response(&mut self, id: u64) -> Value {
            loop {
                let line = self.lines.next_line().await.unwrap().unwrap();
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
            }
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.send_request(method, params).await;
            self.read_response(id).await
        }

        async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
            let response = self
                .request("tools/call", json!({"name": name, "arguments": arguments}))
                .await;
            assert!(response.get("error").is_none(), "{response}");
            response["result"].clone()
        }

        async fn close(mut self) {
            let _ = self.writer.shutdown().await;
        }
    }

    async fn start_test_server<F>(
        server: SottoMcpServer,
        shutdown: F,
    ) -> (TestClient, tokio::task::JoinHandle<Result<()>>)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_transport);
        let server_task = tokio::spawn(async move {
            server
                .serve_transport(server_reader, server_writer, shutdown)
                .await
        });
        let (client_reader, client_writer) = tokio::io::split(client_transport);
        let mut client = TestClient {
            writer: client_writer,
            lines: BufReader::new(client_reader).lines(),
            next_id: 0,
        };
        let initialized = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "sotto-test", "version": "1"}
                }),
            )
            .await;
        assert!(initialized.get("error").is_none(), "{initialized}");
        client
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        (client, server_task)
    }

    #[test]
    fn tools_have_strict_schemas_and_only_the_agreed_names() {
        let mut tools = tool_definitions();
        tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [LIST_TASKS, RUN_TASK]
        );
        for tool in &tools {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        let run_task = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"].as_str() == Some(RUN_TASK))
            .unwrap();
        assert_eq!(
            run_task["inputSchema"]["required"],
            json!(["task", "environment"])
        );
        assert_eq!(
            run_task["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        let output_schema = &run_task["outputSchema"];
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
        let (mut client, server_task) = start_test_server(server, std::future::pending()).await;
        let locked = client
            .call_tool(RUN_TASK, json!({"task":"verify", "environment":"dev"}))
            .await;
        assert_eq!(locked["isError"], true);
        assert_eq!(locked["structuredContent"]["task"], "verify");
        assert_eq!(locked["structuredContent"]["environment"], "dev");
        assert!(locked["structuredContent"]["summary"]
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
            .call_tool(RUN_TASK, json!({"task":"verify", "environment":"dev"}))
            .await;
        assert_eq!(expired["isError"], true);
        assert!(expired["structuredContent"]["summary"]
            .as_str()
            .unwrap()
            .contains("sotto unlock"));
        assert!(!marker.exists());
        client.close().await;
        server_task.await.unwrap().unwrap();
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
        let (mut client, server_task) = start_test_server(server, std::future::pending()).await;
        let listed = client.request("tools/list", json!({})).await;
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 2);

        let result = client
            .call_tool(RUN_TASK, json!({"task":"verify", "environment":"dev"}))
            .await;
        assert_eq!(result["isError"], false);
        assert!(
            marker.exists(),
            "the approved task should receive its canary"
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("synthetic-value"), "{serialized}");
        assert!(!serialized.contains("child output"), "{serialized}");

        let failed = client
            .call_tool(RUN_TASK, json!({"task":"fail", "environment":"dev"}))
            .await;
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["structuredContent"]["status"], "failed");
        assert_eq!(failed["structuredContent"]["exit_code"], 7);
        assert!(!serde_json::to_string(&failed)
            .unwrap()
            .contains("hidden failure output"));

        let timed_out = client
            .call_tool(RUN_TASK, json!({"task":"slow", "environment":"dev"}))
            .await;
        assert_eq!(timed_out["isError"], true);
        assert_eq!(timed_out["structuredContent"]["status"], "timed_out");
        assert_eq!(timed_out["structuredContent"]["exit_code"], Value::Null);

        client.close().await;
        server_task.await.unwrap().unwrap();
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
        let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel::<()>();
        let (mut client, server_task) = start_test_server(server, async move {
            let _ = shutdown_signal.await;
        })
        .await;
        let slow_id = client
            .send_request(
                "tools/call",
                json!({"name": RUN_TASK, "arguments": {"task":"slow", "environment":"dev"}}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task child should start before cancellation");
        client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": slow_id, "reason": "test cancellation"}
            }))
            .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect_err("cancelled task must not complete its post-sleep action");
        assert!(!marker.exists());

        let _active_run = client
            .send_request(
                "tools/call",
                json!({"name": RUN_TASK, "arguments": {"task":"shutdown_slow", "environment":"dev"}}),
            )
            .await;
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
            .unwrap()
            .unwrap();
        assert!(!shutdown_marker.exists());
        client.close().await;
    }

    #[test]
    fn protocol_versions_are_negotiated_against_the_supported_set() {
        for supported in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            assert_eq!(negotiate_protocol_version(Some(supported)), supported);
        }
        assert_eq!(
            negotiate_protocol_version(Some("1999-01-01")),
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(
            negotiate_protocol_version(Some("2026-07-28")),
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol_version(None), DEFAULT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn non_object_tool_arguments_are_rejected_instead_of_emptied() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        let server = SottoMcpServer::new(
            root.path().to_path_buf(),
            store,
            keychain,
            ApprovalStore::open(data.path()).unwrap(),
        );
        let (mut client, server_task) = start_test_server(server, std::future::pending()).await;
        let id = client
            .send_request("tools/call", json!({"name": LIST_TASKS, "arguments": []}))
            .await;
        let response = client.read_response(id).await;
        assert_eq!(response["error"]["code"], CODE_INVALID_PARAMS);
        assert!(response.get("result").is_none(), "{response}");
        client.close().await;
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn requests_without_the_jsonrpc_version_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        let server = SottoMcpServer::new(
            root.path().to_path_buf(),
            store,
            keychain,
            ApprovalStore::open(data.path()).unwrap(),
        );
        let (mut client, server_task) = start_test_server(server, std::future::pending()).await;
        client
            .send(json!({"id": 99, "method": "tools/list", "params": {}}))
            .await;
        let response = client.read_response(99).await;
        assert_eq!(response["error"]["code"], CODE_INVALID_REQUEST);
        assert!(response.get("result").is_none(), "{response}");
        client.close().await;
        server_task.await.unwrap().unwrap();
    }

    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            _buffer: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test broken pipe",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_failed_response_write_terminates_the_server() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        let server = SottoMcpServer::new(
            root.path().to_path_buf(),
            store,
            keychain,
            ApprovalStore::open(data.path()).unwrap(),
        );
        let (client_transport, server_reader) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve_transport(server_reader, FailingWriter, std::future::pending())
                .await
        });
        let (_client_reader, mut client_writer) = tokio::io::split(client_transport);
        client_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"sotto-test\",\"version\":\"1\"}}}\n")
            .await
            .unwrap();
        client_writer.flush().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("a failed write should end the server")
            .unwrap()
            .expect_err("a failed write should be reported");
        assert!(
            error.to_string().contains("writing MCP response"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_message_stops_the_active_child_before_returning_its_error() {
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
        let project = Vault::create_project(&store, &keypair, "MCP oversize test").unwrap();
        let config = crate::config::Config {
            project_id: project.id,
            project: "MCP oversize test".into(),
            environment: "dev".into(),
            org_id: None,
        };
        config.save_to(root.path()).unwrap();
        let started = root.path().join("started");
        let marker = root.path().join("should-not-finish");
        let manifest = format!(
            "version = 1\n\n[[task]]\nname = \"slow\"\ndescription = \"Oversize test\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}]\nenvironments = [\"dev\"]\n",
            format!("touch {:?}; sleep 1; touch {:?}", started, marker),
        );
        std::fs::write(root.path().join("sotto.tasks.toml"), manifest).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let manifest = TaskManifest::load_from(&canonical_root).unwrap();
        let approvals = ApprovalStore::open(data.path()).unwrap();
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
        let (mut client, server_task) = start_test_server(server, std::future::pending()).await;
        client
            .send_request(
                "tools/call",
                json!({"name": RUN_TASK, "arguments": {"task":"slow", "environment":"dev"}}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task child should start before the oversized message");
        client
            .writer
            .write_all("x".repeat(MAX_MESSAGE_BYTES + 1).as_bytes())
            .await
            .unwrap();
        client.writer.write_all(b"\n").await.unwrap();
        client.writer.flush().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("oversized message should end the server")
            .unwrap()
            .expect_err("oversized message should be reported");
        assert!(error.to_string().contains("size limit"), "{error}");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect_err("the stopped child must not complete its post-sleep action");
        assert!(!marker.exists());
        client.close().await;
    }
}
