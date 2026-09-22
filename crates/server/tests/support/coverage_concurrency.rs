use std::{any::Any, future::Future, pin::Pin};

use futures_util::FutureExt;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration, Instant};

/// The race tests deliberately use a finite budget. A blocked backend must never
/// leave an integration test waiting indefinitely when a scenario fails.
pub const RACE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OwnedTask<T> {
    receiver: oneshot::Receiver<T>,
}

pub struct RaceTaskOwner {
    handles: Vec<Option<JoinHandle<()>>>,
    cleanups: Vec<CleanupCallback>,
}

pub type ScenarioFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

type CleanupCallback = Box<dyn FnOnce() -> CleanupFuture + Send>;
type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

impl RaceTaskOwner {
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
            cleanups: Vec::new(),
        }
    }

    pub fn register_cleanup<F, Fut>(&mut self, cleanup: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.cleanups.push(Box::new(move || Box::pin(cleanup())));
    }

    pub fn spawn<T, F>(&mut self, future: F) -> OwnedTask<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.handles.push(Some(tokio::spawn(async move {
            let output = future.await;
            let _ = sender.send(output);
        })));
        OwnedTask { receiver }
    }

    pub async fn abort_and_join(&mut self) -> Result<(), String> {
        self.abort_and_join_within(RACE_TIMEOUT).await
    }

    pub async fn abort_and_join_within(&mut self, budget: Duration) -> Result<(), String> {
        for handle in self.handles.iter().flatten() {
            handle.abort();
        }
        self.settle_within(budget, true).await
    }

    pub async fn join_all_within(&mut self, budget: Duration) -> Result<(), String> {
        self.settle_within(budget, false).await
    }

    pub async fn cleanup_registered(&mut self) -> Result<(), String> {
        self.cleanup_registered_within(RACE_TIMEOUT).await
    }

    async fn cleanup_registered_within(&mut self, budget: Duration) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        let mut failures = Vec::new();
        for cleanup in self.cleanups.drain(..) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
                Ok(future) => future,
                Err(panic) => {
                    failures.push(format!("cleanup panicked: {}", panic_message(panic)));
                    continue;
                }
            };
            let result = timeout(
                remaining,
                std::panic::AssertUnwindSafe(future).catch_unwind(),
            )
            .await;
            match result {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => failures.push(error),
                Ok(Err(panic)) => {
                    failures.push(format!("cleanup panicked: {}", panic_message(panic)))
                }
                Err(_) => failures.push("cleanup timed out".into()),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    async fn settle_within(
        &mut self,
        budget: Duration,
        cancellation_is_expected: bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        let mut failures = Vec::new();
        for slot in &mut self.handles {
            let Some(handle) = slot.as_mut() else {
                continue;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            match timeout(remaining, &mut *handle).await {
                Ok(Ok(())) => *slot = None,
                Ok(Err(error)) if cancellation_is_expected && error.is_cancelled() => {
                    *slot = None;
                }
                Ok(Err(error)) => {
                    *slot = None;
                    failures.push(error.to_string());
                }
                Err(_) => failures.push("timed out joining owned race task".into()),
            }
        }
        self.handles.retain(Option::is_some);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

impl Drop for RaceTaskOwner {
    fn drop(&mut self) {
        for handle in self.handles.iter().flatten() {
            handle.abort();
        }
    }
}

pub async fn receive_owned<T>(
    task: &mut Option<OwnedTask<T>>,
    label: &'static str,
) -> Result<T, String> {
    let task = task
        .take()
        .ok_or_else(|| format!("{label} task was already consumed"))?;
    timeout(RACE_TIMEOUT, task.receiver)
        .await
        .map_err(|_| format!("timed out waiting for {label}"))?
        .map_err(|_| format!("{label} task exited before reporting its result"))
}

pub async fn run_with_teardown<T, F, C, CF>(
    owner: &mut RaceTaskOwner,
    scenario: F,
    cleanup: C,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
    C: FnOnce() -> CF,
    CF: Future<Output = Result<(), String>>,
{
    run_with_teardown_with_budgets(owner, scenario, cleanup, RACE_TIMEOUT, RACE_TIMEOUT).await
}

pub async fn run_with_context<T, C, CF>(
    owner: &mut RaceTaskOwner,
    scenario: impl for<'a> FnOnce(&'a mut RaceTaskOwner) -> ScenarioFuture<'a, T>,
    cleanup: C,
) -> Result<T, String>
where
    C: FnOnce() -> CF,
    CF: Future<Output = Result<(), String>>,
{
    run_with_context_with_budgets(owner, scenario, cleanup, RACE_TIMEOUT, RACE_TIMEOUT).await
}

pub async fn run_with_context_with_budgets<T, C, CF>(
    owner: &mut RaceTaskOwner,
    scenario: impl for<'a> FnOnce(&'a mut RaceTaskOwner) -> ScenarioFuture<'a, T>,
    cleanup: C,
    scenario_budget: Duration,
    teardown_budget: Duration,
) -> Result<T, String>
where
    C: FnOnce() -> CF,
    CF: Future<Output = Result<(), String>>,
{
    let scenario_result =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scenario(owner))) {
            Ok(scenario) => match timeout(
                scenario_budget,
                std::panic::AssertUnwindSafe(scenario).catch_unwind(),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(panic)) => Err(format!("scenario panicked: {}", panic_message(panic))),
                Err(_) => Err("scenario timed out".into()),
            },
            Err(panic) => Err(format!("scenario panicked: {}", panic_message(panic))),
        };
    // The completed scenario result is already available, so zero lets Tokio poll it
    // immediately before teardown begins while preserving the shared teardown path.
    run_with_teardown_with_budgets(
        owner,
        async move { scenario_result },
        cleanup,
        Duration::ZERO,
        teardown_budget,
    )
    .await
}

pub async fn run_with_teardown_with_budgets<T, F, C, CF>(
    owner: &mut RaceTaskOwner,
    scenario: F,
    cleanup: C,
    scenario_budget: Duration,
    teardown_budget: Duration,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
    C: FnOnce() -> CF,
    CF: Future<Output = Result<(), String>>,
{
    let outcome = timeout(
        scenario_budget,
        std::panic::AssertUnwindSafe(scenario).catch_unwind(),
    )
    .await;
    let scenario_result = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) => Err(format!("scenario panicked: {}", panic_message(panic))),
        Err(_) => Err("scenario timed out".into()),
    };
    let task_result = if scenario_result.is_ok() {
        match owner.join_all_within(teardown_budget).await {
            Ok(()) => Ok(()),
            Err(error) => match owner.abort_and_join_within(teardown_budget).await {
                Ok(()) => Err(error),
                Err(abort_error) => Err(format!("{error}; {abort_error}")),
            },
        }
    } else {
        owner.abort_and_join_within(teardown_budget).await
    };
    let registered_cleanup = owner.cleanup_registered_within(teardown_budget).await;
    let explicit_cleanup = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
        Ok(future) => match timeout(
            teardown_budget,
            std::panic::AssertUnwindSafe(future).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(panic)) => Err(format!("cleanup panicked: {}", panic_message(panic))),
            Err(_) => Err("cleanup timed out".into()),
        },
        Err(panic) => Err(format!("cleanup panicked: {}", panic_message(panic))),
    };
    let cleanup_result = match (registered_cleanup, explicit_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(registered), Ok(())) | (Ok(()), Err(registered)) => Err(registered),
        (Err(registered), Err(explicit)) => Err(format!("{registered}; {explicit}")),
    };
    match (scenario_result, task_result, cleanup_result) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (scenario, tasks, cleanup) => Err(format_failure(scenario, tasks, cleanup)),
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => match panic.downcast::<&'static str>() {
            Ok(message) => (*message).into(),
            Err(_) => "non string panic payload".into(),
        },
    }
}

fn format_failure<T>(
    scenario: Result<T, String>,
    tasks: Result<(), String>,
    cleanup: Result<(), String>,
) -> String {
    let mut failures = Vec::new();
    if let Err(error) = scenario {
        failures.push(format!("scenario: {error}"));
    }
    if let Err(error) = tasks {
        failures.push(format!("tasks: {error}"));
    }
    if let Err(error) = cleanup {
        failures.push(format!("cleanup: {error}"));
    }
    failures.join("; ")
}

pub async fn transaction_pid(tx: &mut Transaction<'_, Postgres>) -> i32 {
    timeout(
        RACE_TIMEOUT,
        sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()").fetch_one(&mut **tx),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out reading transaction backend pid"))
    .unwrap_or_else(|error| panic!("failed to read transaction backend pid: {error}"))
}

pub async fn wait_for_specific_block(pool: &PgPool, waiter_pid: i32, holder_pid: i32) {
    let deadline = Instant::now() + RACE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for backend {waiter_pid} to block on {holder_pid}");
        }
        let blocked = timeout(
            remaining,
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_stat_activity
                     WHERE pid = $1 AND $2 = ANY(pg_blocking_pids(pid))
                 )",
            )
            .bind(waiter_pid)
            .bind(holder_pid)
            .fetch_one(pool),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out inspecting transaction blocking"))
        .unwrap_or_else(|error| panic!("failed to inspect transaction blocking: {error}"));
        if blocked {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

pub async fn receive_pid(receiver: oneshot::Receiver<i32>, label: &'static str) -> i32 {
    timeout(RACE_TIMEOUT, receiver)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap_or_else(|_| panic!("{label} task exited before reporting its backend pid"))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::task::{Context, Poll};
    use tokio::time::Duration;

    use super::{
        receive_owned, run_with_context, run_with_teardown, run_with_teardown_with_budgets,
        RaceTaskOwner,
    };

    struct DropMarker(Arc<AtomicBool>);

    impl Future for DropMarker {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn owner_drains_a_panicking_child_and_a_parked_sibling() {
        let mut owner = RaceTaskOwner::new();
        let mut failed = Some(owner.spawn(async {
            panic!("intentional child failure");
        }));
        let mut sibling = Some(owner.spawn(async {
            std::future::pending::<()>().await;
        }));
        let failed_result = receive_owned(&mut failed, "failed child").await;
        assert!(failed_result.is_err());
        let teardown = owner.abort_and_join().await;
        assert!(teardown.is_err());
        assert!(receive_owned(&mut sibling, "parked sibling").await.is_err());
    }

    #[tokio::test]
    async fn scenario_teardown_reports_panic_and_cleanup_failure_together() {
        let mut owner = RaceTaskOwner::new();
        let result = run_with_teardown(
            &mut owner,
            async {
                panic!("intentional scenario failure");
                #[allow(unreachable_code)]
                Ok::<(), String>(())
            },
            || async { Err::<(), _>("intentional cleanup failure".into()) },
        )
        .await;
        assert_eq!(
            result,
            Err("scenario: scenario panicked: intentional scenario failure; cleanup: intentional cleanup failure".into())
        );
    }

    #[tokio::test]
    async fn scenario_teardown_preserves_panic_payload() {
        let mut owner = RaceTaskOwner::new();
        let result = run_with_teardown(
            &mut owner,
            async {
                panic!("readiness never arrived");
                #[allow(unreachable_code)]
                Ok::<(), String>(())
            },
            || async { Ok::<(), String>(()) },
        )
        .await;
        assert_eq!(
            result,
            Err("scenario: scenario panicked: readiness never arrived".into())
        );
    }

    #[tokio::test]
    async fn teardown_bounds_scenario_and_cleanup_phases() {
        let mut owner = RaceTaskOwner::new();
        let timed_out_scenario = run_with_teardown_with_budgets(
            &mut owner,
            async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok::<(), String>(())
            },
            || async { Ok::<(), String>(()) },
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(
            timed_out_scenario,
            Err("scenario: scenario timed out".into())
        );

        let mut owner = RaceTaskOwner::new();
        let timed_out_cleanup = run_with_teardown_with_budgets(
            &mut owner,
            async { Ok::<(), String>(()) },
            || async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok::<(), String>(())
            },
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(timed_out_cleanup, Err("cleanup: cleanup timed out".into()));
    }

    #[tokio::test]
    async fn registered_cleanup_runs_after_a_successful_scenario() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut owner = RaceTaskOwner::new();
        let cleaned_by_callback = Arc::clone(&cleaned);
        owner.register_cleanup(move || async move {
            cleaned_by_callback.store(true, Ordering::SeqCst);
            Ok(())
        });

        run_with_teardown(&mut owner, async { Ok::<(), String>(()) }, || async {
            Ok::<(), String>(())
        })
        .await
        .expect("successful scenario cleanup");
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn context_can_spawn_during_the_supervised_scenario() {
        let mut owner = RaceTaskOwner::new();
        let result = run_with_context(
            &mut owner,
            |owner| {
                Box::pin(async move {
                    let task = owner.spawn(async { 7 });
                    let mut task = Some(task);
                    receive_owned(&mut task, "context child").await
                })
            },
            || async { Ok::<(), String>(()) },
        )
        .await;
        assert_eq!(result, Ok(7));
    }

    #[tokio::test]
    async fn context_panic_aborts_children_and_runs_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut owner = RaceTaskOwner::new();
        let cleaned_by_callback = Arc::clone(&cleaned);
        owner.register_cleanup(move || async move {
            cleaned_by_callback.store(true, Ordering::SeqCst);
            Err("context cleanup failure".into())
        });
        let result = run_with_context(
            &mut owner,
            |owner| {
                Box::pin(async move {
                    let _task = owner.spawn(async { std::future::pending::<()>().await });
                    panic!("context scenario failure");
                    #[allow(unreachable_code)]
                    Ok::<(), String>(())
                })
            },
            || async { Ok::<(), String>(()) },
        )
        .await;
        assert_eq!(
            result,
            Err("scenario: scenario panicked: context scenario failure; cleanup: context cleanup failure".into())
        );
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn setup_cleanup_can_be_drained_before_supervised_teardown() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut owner = RaceTaskOwner::new();
        let cleaned_by_callback = Arc::clone(&cleaned);
        owner.register_cleanup(move || async move {
            cleaned_by_callback.store(true, Ordering::SeqCst);
            Ok(())
        });

        owner
            .cleanup_registered()
            .await
            .expect("drain setup cleanup");
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cleanup_invocation_panics_are_reported_and_teardown_continues() {
        let mut owner = RaceTaskOwner::new();
        owner.register_cleanup(|| {
            panic!("registered cleanup invocation failure");
            #[allow(unreachable_code)]
            std::future::ready(Ok::<(), String>(()))
        });
        let result = run_with_teardown(&mut owner, async { Ok::<(), String>(()) }, || {
            panic!("explicit cleanup invocation failure");
            #[allow(unreachable_code)]
            std::future::ready(Ok::<(), String>(()))
        })
        .await;
        assert_eq!(
            result,
            Err("cleanup: cleanup panicked: registered cleanup invocation failure; cleanup panicked: explicit cleanup invocation failure".into())
        );
    }

    #[tokio::test]
    async fn child_failure_before_readiness_cancels_a_parked_sibling() {
        let mut owner = RaceTaskOwner::new();
        let mut failed = Some(owner.spawn(async {
            panic!("child failed before readiness");
        }));
        let _sibling = owner.spawn(async {
            std::future::pending::<()>().await;
        });
        let result = run_with_teardown(
            &mut owner,
            async {
                receive_owned(&mut failed, "readiness child")
                    .await
                    .map(|_| ())
            },
            || async { Ok::<(), String>(()) },
        )
        .await;
        assert!(matches!(result, Err(error) if error.contains("readiness child task exited")));
    }

    #[tokio::test]
    async fn join_deadline_retains_a_parked_child_until_abort() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut owner = RaceTaskOwner::new();
        let dropped_by_child = Arc::clone(&dropped);
        let _task = owner.spawn(DropMarker(dropped_by_child));

        let result = owner.join_all_within(Duration::from_millis(10)).await;
        assert!(result.is_err());
        assert!(!dropped.load(Ordering::SeqCst));

        owner.abort_and_join().await.expect("abort retained child");
        assert!(dropped.load(Ordering::SeqCst));
    }
}
