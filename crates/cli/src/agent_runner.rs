//! Shared execution boundary for developer-approved project tasks.
//!
//! The runner reloads declarations and approvals immediately before launch, injects secrets
//! through the normal Sotto command layer, and never returns or logs child output.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::agent_approvals::{approval_fingerprint, project_root_key, ApprovalStore};
use crate::agent_tasks::{TaskManifest, DEFAULT_TASK_TIMEOUT_SECONDS, MAX_TASK_TIMEOUT_SECONDS};
use crate::commands::App;
use crate::config::Config;
use crate::error::{Error, Result};

const CREDENTIAL_ENVIRONMENT: [&str; 3] = ["SOTTO_TOKEN", "SOTTO_PASSWORD", "SOTTO_SECRET_KEY"];

/// A process outcome safe to return to an agent. It contains no stdout, stderr, secret values,
/// executable error text, or other child-controlled content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunOutcome {
    pub task_name: String,
    pub environment: String,
    pub status: TaskRunStatus,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRunStatus {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    CouldNotStart,
    WaitFailed,
}

impl TaskRunOutcome {
    pub fn summary(&self) -> String {
        match self.status {
            TaskRunStatus::Completed => format!(
                "task {:?} completed successfully in environment {:?}",
                self.task_name, self.environment
            ),
            TaskRunStatus::Failed => match self.exit_code {
                Some(code) => format!(
                    "task {:?} failed with exit code {code} in environment {:?}",
                    self.task_name, self.environment
                ),
                None => format!(
                    "task {:?} failed without an exit code in environment {:?}",
                    self.task_name, self.environment
                ),
            },
            TaskRunStatus::TimedOut => format!(
                "task {:?} timed out in environment {:?}",
                self.task_name, self.environment
            ),
            TaskRunStatus::Cancelled => format!(
                "task {:?} was cancelled in environment {:?}",
                self.task_name, self.environment
            ),
            TaskRunStatus::CouldNotStart => format!(
                "task {:?} could not be started in environment {:?}",
                self.task_name, self.environment
            ),
            TaskRunStatus::WaitFailed => format!(
                "task {:?} could not be monitored in environment {:?}",
                self.task_name, self.environment
            ),
        }
    }
}

/// Run one task after revalidating its declaration, executable and exact local approval.
///
/// The caller owns unlock UX and must pass a cancellation receiver. A locked or expired session
/// fails through `App::entries` without prompting. This API is shared by the CLI now and the
/// local MCP transport in a later change.
pub fn run_approved_task(
    app: &App<'_>,
    approvals: &ApprovalStore,
    start_path: &Path,
    task_name: &str,
    environment: &str,
    cancellation: watch::Receiver<bool>,
) -> Result<impl Future<Output = Result<TaskRunOutcome>> + Send + 'static> {
    let prepared = prepare_task(app, approvals, start_path, task_name, environment)?;
    Ok(execute(prepared, cancellation))
}

struct PreparedTask {
    task_name: String,
    environment: String,
    executable: PathBuf,
    args: Vec<String>,
    working_directory: PathBuf,
    timeout: Duration,
    secrets: Vec<(String, Zeroizing<String>)>,
}

fn prepare_task(
    app: &App<'_>,
    approvals: &ApprovalStore,
    start_path: &Path,
    task_name: &str,
    environment: &str,
) -> Result<PreparedTask> {
    if environment.is_empty() {
        return Err(Error::Input(
            "task execution requires an explicit non-empty --env <ENVIRONMENT>".into(),
        ));
    }
    let (mut config, discovered_root) = Config::discover(start_path)?;
    let project_root = std::fs::canonicalize(&discovered_root)
        .map_err(|error| Error::Io(format!("resolving project root: {error}")))?;
    config.environment = environment.to_owned();

    // Always re-read the checked-in declaration at execution time, then resolve exactly the
    // executable whose canonical path is included in the PR2 approval fingerprint.
    let manifest = TaskManifest::load_from(&project_root)?;
    let task = manifest
        .tasks()
        .iter()
        .find(|task| task.name() == task_name)
        .ok_or_else(|| Error::NotFound(format!("agent task `{task_name}`")))?;
    if !task
        .environments()
        .iter()
        .any(|allowed| allowed == environment)
    {
        return Err(Error::Input(format!(
            "task {task_name:?} does not allow environment {environment:?}"
        )));
    }
    let executable = task.resolve_executable(&project_root)?;
    let declaration_fingerprint = manifest.fingerprint_for(task_name)?;
    let fingerprint = approval_fingerprint(&declaration_fingerprint, &executable)?;
    let project_root_key = project_root_key(&project_root);
    if !approvals.is_approved(
        &config.project_id,
        &project_root_key,
        task_name,
        environment,
        &fingerprint,
    )? {
        return Err(Error::Input(format!(
            "execution denied: task {task_name:?} for environment {environment:?} has no current local approval; review the current task declaration and resolved executable, then run `sotto task approve {task_name} --env {environment}`"
        )));
    }

    // Decryption uses the existing command layer and session check. No prompt is attempted here.
    let mut secrets = Vec::new();
    for (name, bytes) in app.entries(&config)? {
        validate_environment_key(&name)?;
        if is_sotto_credential_environment_name(&name) {
            return Err(Error::Input(format!(
                "secret name {name:?} is reserved for Sotto credentials and cannot be injected into a task"
            )));
        }
        let value = String::from_utf8(bytes).map_err(|_| {
            Error::Input(format!(
                "secret {name:?} is not valid UTF-8; cannot inject it into a task"
            ))
        })?;
        secrets.push((name, Zeroizing::new(value)));
    }

    let requested_timeout = task
        .timeout_seconds()
        .unwrap_or(DEFAULT_TASK_TIMEOUT_SECONDS);
    if requested_timeout == 0 || requested_timeout > MAX_TASK_TIMEOUT_SECONDS {
        return Err(Error::Config(format!(
            "task timeout must be between 1 and {MAX_TASK_TIMEOUT_SECONDS} seconds"
        )));
    }

    Ok(PreparedTask {
        task_name: task.name().to_owned(),
        environment: environment.to_owned(),
        executable,
        args: task.args().to_vec(),
        working_directory: task.working_directory(&project_root),
        timeout: Duration::from_secs(requested_timeout),
        secrets,
    })
}

fn validate_environment_key(key: &str) -> Result<()> {
    let mut chars = key.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::Input(format!(
            "secret name {key:?} is not a usable environment variable name (expected [A-Za-z_][A-Za-z0-9_]*)"
        )))
    }
}

fn is_sotto_credential_environment_name(name: &str) -> bool {
    CREDENTIAL_ENVIRONMENT
        .iter()
        .any(|credential| name.eq_ignore_ascii_case(credential))
}

async fn execute(
    prepared: PreparedTask,
    mut cancellation: watch::Receiver<bool>,
) -> Result<TaskRunOutcome> {
    if *cancellation.borrow() {
        return Ok(outcome(&prepared, TaskRunStatus::Cancelled, None));
    }
    let mut command = Command::new(&prepared.executable);
    command
        .args(&prepared.args)
        .current_dir(&prepared.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for credential in CREDENTIAL_ENVIRONMENT {
        command.env_remove(credential);
    }
    for (name, value) in &prepared.secrets {
        command.env(name, OsString::from(value.as_str()));
    }
    configure_process_group(&mut command);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Ok(outcome(&prepared, TaskRunStatus::CouldNotStart, None));
        }
    };
    let mut child = RunningChild { process: child };
    let drains = drain_child_output(&mut child.process);

    let timeout = tokio::time::sleep(prepared.timeout);
    tokio::pin!(timeout);
    let status = loop {
        tokio::select! {
            result = child.process.wait() => {
                break match result {
                    Ok(status) if status.success() => TaskRunStatus::Completed,
                    Ok(_) => TaskRunStatus::Failed,
                    Err(_) => {
                        terminate_child(&mut child.process).await;
                        TaskRunStatus::WaitFailed
                    }
                };
            }
            _ = &mut timeout => {
                terminate_child(&mut child.process).await;
                break TaskRunStatus::TimedOut;
            }
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    terminate_child(&mut child.process).await;
                    break TaskRunStatus::Cancelled;
                }
            }
        }
    };

    // Termination and pipe closure can race with the drain workers. Their only job is to read
    // and drop bytes, so aborting them cannot leak or expose output and keeps shutdown bounded.
    drains.abort();

    let exit_code = if status == TaskRunStatus::Completed || status == TaskRunStatus::Failed {
        child
            .process
            .try_wait()
            .ok()
            .flatten()
            .and_then(|status| status.code())
    } else {
        None
    };
    Ok(outcome(&prepared, status, exit_code))
}

/// Ensure dropping the execution future also tears down its child. Explicit timeout/cancellation
/// paths await the child; this guard covers caller cancellation and runtime shutdown.
struct RunningChild {
    process: Child,
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.process.start_kill();

        // Tokio's `Child` documentation requires a successful `wait` or `try_wait` to reap a
        // Unix child after `start_kill`. The normal async path already waited; this poll handles
        // callers dropping the execution future while the process is active.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.process.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if std::time::Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

fn outcome(
    prepared: &PreparedTask,
    status: TaskRunStatus,
    exit_code: Option<i32>,
) -> TaskRunOutcome {
    TaskRunOutcome {
        task_name: prepared.task_name.clone(),
        environment: prepared.environment.clone(),
        status,
        exit_code,
    }
}

struct OutputDrains(Vec<tokio::task::JoinHandle<io::Result<u64>>>);

impl OutputDrains {
    fn abort(&self) {
        for drain in &self.0 {
            drain.abort();
        }
    }
}

impl Drop for OutputDrains {
    fn drop(&mut self) {
        self.abort();
    }
}

fn drain_child_output(child: &mut Child) -> OutputDrains {
    let mut drains = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        drains.push(tokio::spawn(async move {
            let mut stream = stdout;
            let mut buffer = [0u8; 8192];
            let mut count = 0u64;
            loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Ok(count);
                }
                count = count.saturating_add(read as u64);
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        drains.push(tokio::spawn(async move {
            let mut stream = stderr;
            let mut buffer = [0u8; 8192];
            let mut count = 0u64;
            loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Ok(count);
                }
                count = count.saturating_add(read as u64);
            }
        }));
    }
    OutputDrains(drains)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_approvals::ApprovalStore;
    use crate::commands::App;
    use crate::keychain::MemoryKeychain;
    use crate::session;
    use crate::store::Store;
    use crate::vault::Vault;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_output_drains_aborts_reader_tasks() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (_writer, mut reader) = tokio::io::duplex(1);
        let marker = DropMarker(Arc::clone(&dropped));
        let drain = tokio::spawn(async move {
            let _marker = marker;
            let mut buffer = [0u8; 8];
            let mut count = 0u64;
            loop {
                let read = reader.read(&mut buffer).await?;
                if read == 0 {
                    return Ok(count);
                }
                count += read as u64;
            }
        });
        let drains = OutputDrains(vec![drain]);
        tokio::task::yield_now().await;

        drop(drains);
        tokio::task::yield_now().await;

        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn reserved_sotto_credential_names_are_case_insensitive() {
        for name in [
            "SOTTO_TOKEN",
            "sotto_token",
            "Sotto_Token",
            "SOTTO_PASSWORD",
            "sotto_password",
            "SOTTO_SECRET_KEY",
            "sotto_secret_key",
        ] {
            assert!(is_sotto_credential_environment_name(name), "{name}");
        }
        assert!(!is_sotto_credential_environment_name("SOTTO_TOKEN_EXTRA"));
        assert!(!is_sotto_credential_environment_name("TOKEN"));
    }

    fn fixture(root: &Path, data: &Path) -> (Store, MemoryKeychain, Config, ApprovalStore) {
        std::fs::create_dir_all(root).unwrap();
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
        let project = Vault::create_project(&store, &keypair, "runner test").unwrap();
        let config = Config {
            project_id: project.id,
            project: "runner test".into(),
            environment: "dev".into(),
            org_id: None,
        };
        config.save_to(root).unwrap();
        let approvals = ApprovalStore::open(data).unwrap();
        (store, keychain, config, approvals)
    }

    #[cfg(unix)]
    fn write_task(root: &Path, script: &str, timeout_seconds: Option<u64>) {
        let timeout = timeout_seconds
            .map(|seconds| format!("timeout_seconds = {seconds}\n"))
            .unwrap_or_default();
        let manifest = format!(
            "version = 1\n\n[[task]]\nname = \"verify\"\ndescription = \"Test runner\"\nexecutable = \"/bin/sh\"\nargs = [\"-c\", {:?}, \"task\", \"fixed argument\"]\nenvironments = [\"dev\"]\n{timeout}",
            script
        );
        std::fs::write(root.join("sotto.tasks.toml"), manifest).unwrap();
    }

    #[cfg(unix)]
    fn approve(root: &Path, config: &Config, approvals: &ApprovalStore) {
        let root = std::fs::canonicalize(root).unwrap();
        let manifest = TaskManifest::load_from(&root).unwrap();
        let task = &manifest.tasks()[0];
        let executable = task.resolve_executable(&root).unwrap();
        let fingerprint =
            approval_fingerprint(&manifest.fingerprint_for("verify").unwrap(), &executable)
                .unwrap();
        approvals
            .approve(
                &config.project_id,
                &project_root_key(&root),
                "verify",
                "dev",
                &fingerprint,
            )
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approved_task_gets_selected_secrets_fixed_arguments_and_cwd_without_returning_output()
    {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let marker = root.path().join("result.txt");
        let script = format!(
            "printf '%s|%s|%s|%s|%s' \"$CANARY\" \"$MISSING_CANARY\" \"$PWD\" \"$1\" \"$SOTTO_TOKEN\" > {:?}; printf 'secret child output'",
            marker
        );
        write_task(root.path(), &script, Some(5));
        let app = App::new(&store, &keychain);
        app.set(&config, "CANARY", b"synthetic-value").unwrap();
        approve(root.path(), &config, &approvals);
        let (_cancel, receiver) = watch::channel(false);

        let result = run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver)
            .unwrap()
            .await
            .unwrap();

        assert_eq!(result.status, TaskRunStatus::Completed);
        assert!(!result.summary().contains("synthetic-value"));
        assert!(!result.summary().contains("secret child output"));
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            format!(
                "synthetic-value||{}|fixed argument|",
                canonical_root.display()
            )
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_approval_and_stale_declaration_do_not_spawn() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let marker = root.path().join("spawned");
        let script = format!("touch {:?}", marker);
        write_task(root.path(), &script, None);
        let app = App::new(&store, &keychain);
        let (_cancel, receiver) = watch::channel(false);
        let environment_error =
            match run_approved_task(&app, &approvals, root.path(), "verify", "prod", receiver) {
                Err(error) => error,
                Ok(_) => panic!("task ran in a disallowed environment"),
            };
        assert!(environment_error
            .to_string()
            .contains("does not allow environment"));
        assert!(!marker.exists());
        let (_cancel, receiver) = watch::channel(false);
        let error =
            match run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver) {
                Err(error) => error,
                Ok(_) => panic!("task ran without an approval"),
            };
        assert!(error
            .to_string()
            .contains("sotto task approve verify --env dev"));
        assert!(!marker.exists());

        approve(root.path(), &config, &approvals);
        write_task(root.path(), &format!("touch {:?}; exit 0", marker), None);
        let (_cancel, receiver) = watch::channel(false);
        assert!(
            run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver).is_err()
        );
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_and_cancellation_terminate_the_child() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let marker = root.path().join("finished");
        let script = format!("sleep 10; touch {:?}", marker);
        write_task(root.path(), &script, Some(1));
        approve(root.path(), &config, &approvals);
        let app = App::new(&store, &keychain);
        let (_cancel, receiver) = watch::channel(false);
        let timed_out = run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(timed_out.status, TaskRunStatus::TimedOut);
        assert!(!marker.exists());

        write_task(root.path(), &script, Some(5));
        approve(root.path(), &config, &approvals);
        let (cancel, receiver) = watch::channel(false);
        let run =
            run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver).unwrap();
        tokio::pin!(run);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.send(true).unwrap();
        let cancelled = run.await.unwrap();
        assert_eq!(cancelled.status, TaskRunStatus::Cancelled);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_runner_future_terminates_the_process_group() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let marker = root.path().join("finished");
        let pid_file = root.path().join("pid");
        let script = format!("echo $$ > {:?}; sleep 1; touch {:?}", pid_file, marker);
        write_task(root.path(), &script, Some(5));
        approve(root.path(), &config, &approvals);
        let app = App::new(&store, &keychain);
        let (_cancel, receiver) = watch::channel(false);

        {
            let run = run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver)
                .unwrap();
            tokio::pin!(run);
            tokio::select! {
                _ = &mut run => panic!("task completed before it could be cancelled"),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }

        let child_pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(child_pid), None).is_err());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn locked_session_fails_before_spawn_and_large_output_is_discarded() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let marker = root.path().join("spawned");
        let script = format!(
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2; touch {:?}; exit 7",
            marker
        );
        write_task(root.path(), &script, Some(5));
        approve(root.path(), &config, &approvals);
        let app = App::new(&store, &keychain);

        session::lock(&keychain).unwrap();
        let (_cancel, receiver) = watch::channel(false);
        assert!(matches!(
            run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver),
            Err(Error::Locked)
        ));
        assert!(!marker.exists());

        session::unlock(
            &store,
            &keychain,
            b"test password",
            Duration::from_secs(3600),
        )
        .unwrap();
        let (_cancel, receiver) = watch::channel(false);
        let outcome = run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(outcome.status, TaskRunStatus::Failed);
        assert_eq!(outcome.exit_code, Some(7));
        assert!(marker.exists());
        assert!(!outcome.summary().contains("synthetic"));

        std::fs::remove_file(&marker).unwrap();
        session::lock(&keychain).unwrap();
        session::unlock(
            &store,
            &keychain,
            b"test password",
            Duration::from_millis(1),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (_cancel, receiver) = watch::channel(false);
        assert!(matches!(
            run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver),
            Err(Error::Locked)
        ));
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_start_failure_is_returned_as_a_typed_outcome() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (store, keychain, config, approvals) = fixture(root.path(), data.path());
        let executable = root.path().join("task-bin");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(
            root.path().join("sotto.tasks.toml"),
            "version = 1\n\n[[task]]\nname = \"verify\"\ndescription = \"Test runner\"\nexecutable = \"./task-bin\"\nargs = []\nenvironments = [\"dev\"]\n",
        )
        .unwrap();
        let manifest = TaskManifest::load_from(root.path()).unwrap();
        let resolved = manifest.tasks()[0].resolve_executable(root.path()).unwrap();
        let fingerprint =
            approval_fingerprint(&manifest.fingerprint_for("verify").unwrap(), &resolved).unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        approvals
            .approve(
                &config.project_id,
                &project_root_key(&canonical_root),
                "verify",
                "dev",
                &fingerprint,
            )
            .unwrap();
        let app = App::new(&store, &keychain);
        let (_cancel, receiver) = watch::channel(false);
        let run =
            run_approved_task(&app, &approvals, root.path(), "verify", "dev", receiver).unwrap();
        std::fs::remove_file(executable).unwrap();

        let outcome = run.await.unwrap();
        assert_eq!(outcome.status, TaskRunStatus::CouldNotStart);
        assert_eq!(outcome.exit_code, None);
    }
}
