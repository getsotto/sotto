//! Provider refresh orchestration acceptance tests.
//!
//! These tests exercise the public refresh operation against PostgreSQL. They are opt-in because
//! the assertions cover committed receipt, attempt and projection state rather than a mock store.

use std::{str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    PgPool,
};
use tokio::{sync::Notify, time::timeout};
use uuid::Uuid;

use sotto_server::cloud_coverage::ConfirmedPaidInterval;
use sotto_server::cloud_coverage_reconciliation::SourceBinding;
use sotto_server::cloud_provider::{
    AllocationState, CollectionLimits, ProviderCollectionError, ProviderContext,
    ProviderEnvironment, ProviderHistoryClient, ProviderHistoryPage, VerifiedAllocation,
    VerifiedProviderEvent,
};
use sotto_server::cloud_provider_refresh::{refresh_verified_event, ProviderRefreshError};
use sotto_server::db;

enum ClientAction {
    Page(ProviderHistoryPage),
    Pages(Vec<ProviderHistoryPage>),
    Fail(String),
    Block {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        page: ProviderHistoryPage,
    },
}

struct ScriptedClient {
    action: ClientAction,
}

#[async_trait]
impl ProviderHistoryClient for ScriptedClient {
    async fn fetch_page(
        &mut self,
        _context: &ProviderContext,
        binding: &SourceBinding,
        _cursor: Option<&str>,
    ) -> Result<ProviderHistoryPage, ProviderCollectionError> {
        match &self.action {
            ClientAction::Page(page) => Ok(page.clone()),
            ClientAction::Pages(pages) => pages
                .iter()
                .find(|page| page.source_id == binding.source_id)
                .cloned()
                .ok_or_else(|| {
                    ProviderCollectionError::Fetch("missing scripted source page".into())
                }),
            ClientAction::Fail(message) => Err(ProviderCollectionError::Fetch(message.clone())),
            ClientAction::Block {
                entered,
                release,
                page,
            } => {
                entered.notify_waiters();
                release.notified().await;
                Ok(page.clone())
            }
        }
    }
}

async fn pool_or_skip(max_connections: u32) -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping provider refresh test: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing provider refresh tests against non-local host: {}",
        options.get_host()
    );
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn context() -> ProviderContext {
    ProviderContext::new("stripe", "acct_refresh_test", ProviderEnvironment::Test).unwrap()
}

struct Fixture {
    beneficiary_id: String,
    payer_id: String,
    allocation_id: String,
    source_id: String,
    event_id: String,
    subscription_id: String,
    external_reference: String,
}

async fn fixture(pool: &PgPool) -> (Fixture, VerifiedProviderEvent, VerifiedAllocation) {
    let suffix = Uuid::new_v4().to_string();
    let fixture = Fixture {
        beneficiary_id: format!("provider-refresh-beneficiary-{suffix}"),
        payer_id: format!("provider-refresh-payer-{suffix}"),
        allocation_id: format!("provider-refresh-allocation-{suffix}"),
        source_id: format!("provider-refresh-source-{suffix}"),
        event_id: format!("provider-refresh-event-{suffix}"),
        subscription_id: format!("provider-refresh-subscription-{suffix}"),
        external_reference: format!("provider-refresh-external-{suffix}"),
    };
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'provider-refresh-test', $1)",
    )
    .bind(&fixture.beneficiary_id)
    .execute(pool)
    .await
    .expect("insert fixture user");
    let event = VerifiedProviderEvent::from_payload(
        &fixture.event_id,
        "invoice.paid",
        1_700_100_000,
        Some(fixture.subscription_id.clone()),
        Some(fixture.external_reference.clone()),
        br#"{"status":"paid"}"#,
    )
    .unwrap();
    let allocation = VerifiedAllocation::new(
        &fixture.allocation_id,
        &fixture.payer_id,
        format!("customer-{}", fixture.beneficiary_id),
        sotto_server::cloud_provider::PayerKind::Personal,
        &fixture.beneficiary_id,
        &fixture.subscription_id,
        "price_cloud",
        &fixture.external_reference,
        &fixture.source_id,
        0,
        None,
        AllocationState::Active,
        format!("ownership-{}", fixture.beneficiary_id),
    )
    .unwrap();
    (fixture, event, allocation)
}

async fn record_event(pool: &PgPool, event: &VerifiedProviderEvent) {
    let mut tx = pool.begin().await.expect("begin event transaction");
    sotto_server::cloud_provider::record_verified_event(&mut tx, &context(), event)
        .await
        .expect("record event");
    tx.commit().await.expect("commit event");
}

fn second_event_and_allocation(
    fixture: &Fixture,
) -> (VerifiedProviderEvent, VerifiedAllocation, String, String) {
    let suffix = Uuid::new_v4().to_string();
    let source_id = format!("provider-refresh-second-source-{suffix}");
    let allocation_id = format!("provider-refresh-second-allocation-{suffix}");
    let subscription_id = format!("provider-refresh-second-subscription-{suffix}");
    let external_reference = format!("provider-refresh-second-external-{suffix}");
    let event_id = format!("provider-refresh-second-event-{suffix}");
    let event = VerifiedProviderEvent::from_payload(
        &event_id,
        "invoice.paid",
        1_700_100_001,
        Some(subscription_id.clone()),
        Some(external_reference.clone()),
        br#"{"status":"paid","source":2}"#,
    )
    .unwrap();
    let allocation = VerifiedAllocation::new(
        &allocation_id,
        &fixture.payer_id,
        format!("customer-{}", fixture.beneficiary_id),
        sotto_server::cloud_provider::PayerKind::Personal,
        &fixture.beneficiary_id,
        &subscription_id,
        "price_cloud",
        &external_reference,
        &source_id,
        0,
        None,
        AllocationState::Active,
        format!("ownership-second-{}", fixture.beneficiary_id),
    )
    .unwrap();
    (event, allocation, event_id, allocation_id)
}

fn page(context: &ProviderContext, source_id: &str) -> ProviderHistoryPage {
    ProviderHistoryPage {
        context: context.clone(),
        source_id: source_id.into(),
        evidence_reference: "provider-page-evidence".into(),
        paid_intervals: vec![ConfirmedPaidInterval {
            coverage_id: format!("provider-refresh-coverage-{source_id}"),
            source_id: source_id.into(),
            starts_at: 0,
            paid_until: 100,
            failed_renewal_id: None,
        }],
        next_cursor: None,
        authoritative_end: true,
    }
}

async fn cleanup(pool: &PgPool, fixture: &Fixture) {
    cleanup_event_and_allocation(pool, &fixture.event_id, &fixture.allocation_id).await;
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(&fixture.beneficiary_id)
        .execute(pool)
        .await
        .expect("delete fixture facts");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL WHERE beneficiary_id = $1",
    )
    .bind(&fixture.beneficiary_id)
    .execute(pool)
    .await
    .expect("clear fixture attempt");
    for table in [
        "cloud_coverage_collection_attempts",
        "cloud_coverage_sources",
        "cloud_coverage_coordinators",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&fixture.beneficiary_id)
            .execute(pool)
            .await
            .expect("delete fixture coordination row");
    }
    sqlx::query(
        "UPDATE cloud_coverage_heads SET current_revision = NULL WHERE beneficiary_id = $1",
    )
    .bind(&fixture.beneficiary_id)
    .execute(pool)
    .await
    .expect("clear fixture head");
    for table in ["cloud_coverage_revisions", "cloud_coverage_heads"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&fixture.beneficiary_id)
            .execute(pool)
            .await
            .expect("delete fixture projection row");
    }
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
        .bind(&fixture.payer_id)
        .execute(pool)
        .await
        .expect("delete fixture payer");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .execute(pool)
        .await
        .expect("delete fixture user");
}

async fn cleanup_event_and_allocation(pool: &PgPool, event_id: &str, allocation_id: &str) {
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(event_id)
        .execute(pool)
        .await
        .expect("delete fixture receipt");
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(allocation_id)
        .execute(pool)
        .await
        .expect("delete fixture allocation");
}

#[tokio::test]
async fn refresh_commits_collection_and_receipt_after_external_fetch() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let context = context();
    let mut client = ScriptedClient {
        action: ClientAction::Page(page(&context, &fixture.source_id)),
    };

    let receipt = refresh_verified_event(
        &pool,
        &mut client,
        &context,
        &event,
        &allocation,
        CollectionLimits::default(),
    )
    .await
    .expect("refresh succeeds");
    assert_eq!(
        receipt.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );
    assert!(receipt.revision > 0);
    let status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&fixture.event_id)
            .fetch_one(&pool)
            .await
            .expect("read receipt");
    assert_eq!(status, "applied");

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn failed_collection_preserves_pending_receipt_and_fresh_retry_applies() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let context = context();
    let mut failed_client = ScriptedClient {
        action: ClientAction::Fail("provider unavailable".into()),
    };
    let failed = refresh_verified_event(
        &pool,
        &mut failed_client,
        &context,
        &event,
        &allocation,
        CollectionLimits::default(),
    )
    .await;
    assert!(matches!(
        failed,
        Err(ProviderRefreshError::Collection(
            ProviderCollectionError::Fetch(_)
        ))
    ));
    let status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&fixture.event_id)
            .fetch_one(&pool)
            .await
            .expect("read pending receipt");
    assert_eq!(status, "pending");

    let mut retry_client = ScriptedClient {
        action: ClientAction::Page(page(&context, &fixture.source_id)),
    };
    let retry = refresh_verified_event(
        &pool,
        &mut retry_client,
        &context,
        &event,
        &allocation,
        CollectionLimits::default(),
    )
    .await
    .expect("fresh retry succeeds");
    assert_eq!(
        retry.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn invalid_limits_do_not_start_preparation_or_provider_fetch() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    let context = context();
    let mut client = ScriptedClient {
        action: ClientAction::Fail("client must not be called".into()),
    };
    let result = refresh_verified_event(
        &pool,
        &mut client,
        &context,
        &event,
        &allocation,
        CollectionLimits {
            max_sources: 0,
            ..CollectionLimits::default()
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(ProviderRefreshError::Collection(
            ProviderCollectionError::InvalidLimits
        ))
    ));
    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&fixture.event_id)
    .fetch_one(&pool)
    .await
    .expect("read receipt count");
    assert_eq!(receipt_count, 0);

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn receipt_update_failure_rolls_back_projection_and_retry_applies_once() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let suffix = Uuid::new_v4().to_string().replace('-', "_");
    let function_name = format!("provider_refresh_fail_{suffix}");
    let trigger_name = format!("provider_refresh_fail_trigger_{suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION public.{function_name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'provider refresh test failure'; END; $$"
    ))
    .execute(&pool)
    .await
    .expect("create receipt failure function");
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger_name} BEFORE UPDATE OF status ON cloud_provider_event_receipts FOR EACH ROW WHEN (NEW.status = 'applied') EXECUTE FUNCTION public.{function_name}()"
    ))
    .execute(&pool)
    .await
    .expect("create receipt failure trigger");

    let context = context();
    let mut client = ScriptedClient {
        action: ClientAction::Page(page(&context, &fixture.source_id)),
    };
    let failed = refresh_verified_event(
        &pool,
        &mut client,
        &context,
        &event,
        &allocation,
        CollectionLimits::default(),
    )
    .await;
    sqlx::query(&format!(
        "DROP TRIGGER {trigger_name} ON cloud_provider_event_receipts"
    ))
    .execute(&pool)
    .await
    .expect("drop receipt failure trigger");
    sqlx::query(&format!("DROP FUNCTION public.{function_name}()"))
        .execute(&pool)
        .await
        .expect("drop receipt failure function");

    assert!(matches!(failed, Err(ProviderRefreshError::Completion(_))));
    let status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&fixture.event_id)
            .fetch_one(&pool)
            .await
            .expect("read rolled back receipt");
    assert_eq!(status, "pending");
    let revision_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_coverage_revisions WHERE beneficiary_id = $1",
    )
    .bind(&fixture.beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("read rolled back revisions");
    assert_eq!(
        revision_count, 1,
        "registration revision survives completion rollback"
    );
    let attempt_status: String = sqlx::query_scalar(
        "SELECT status FROM cloud_coverage_collection_attempts WHERE beneficiary_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&fixture.beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("read rolled back attempt");
    assert_eq!(attempt_status, "pending");

    let mut retry_client = ScriptedClient {
        action: ClientAction::Page(page(&context, &fixture.source_id)),
    };
    let retry = refresh_verified_event(
        &pool,
        &mut retry_client,
        &context,
        &event,
        &allocation,
        CollectionLimits::default(),
    )
    .await
    .expect("retry after rollback succeeds");
    assert_eq!(
        retry.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn rejection_during_fetch_stays_terminal_and_cannot_publish() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let context = context();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut client = ScriptedClient {
        action: ClientAction::Block {
            entered: entered.clone(),
            release: release.clone(),
            page: page(&context, &fixture.source_id),
        },
    };
    let refresh_pool = pool.clone();
    let refresh_context = context.clone();
    let refresh_event = event.clone();
    let refresh_allocation = allocation.clone();
    let entered_wait = entered.notified();
    let task = tokio::spawn(async move {
        refresh_verified_event(
            &refresh_pool,
            &mut client,
            &refresh_context,
            &refresh_event,
            &refresh_allocation,
            CollectionLimits::default(),
        )
        .await
    });
    timeout(Duration::from_secs(5), entered_wait)
        .await
        .expect("provider request entered");

    let mut reject = pool.begin().await.expect("begin rejection");
    sotto_server::cloud_provider::reject_verified_event(
        &mut reject,
        &context,
        &event,
        "unsupported_event",
    )
    .await
    .expect("reject pending event");
    reject.commit().await.expect("commit rejection");
    release.notify_waiters();

    let result = timeout(Duration::from_secs(5), task)
        .await
        .expect("refresh task completes")
        .expect("refresh task joins");
    assert!(matches!(
        result,
        Err(ProviderRefreshError::Completion(
            sotto_server::cloud_provider::ProviderAdapterError::EventNotPending
        ))
    ));
    let status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&fixture.event_id)
            .fetch_one(&pool)
            .await
            .expect("read rejected receipt");
    assert_eq!(status, "rejected");
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn newer_source_registration_supersedes_paused_refresh_and_retries_complete_set() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let (second_event, second_allocation, second_event_id, second_allocation_id) =
        second_event_and_allocation(&fixture);
    record_event(&pool, &second_event).await;
    let context = context();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut paused_client = ScriptedClient {
        action: ClientAction::Block {
            entered: entered.clone(),
            release: release.clone(),
            page: page(&context, &fixture.source_id),
        },
    };
    let refresh_pool = pool.clone();
    let refresh_context = context.clone();
    let refresh_event = event.clone();
    let refresh_allocation = allocation.clone();
    let entered_wait = entered.notified();
    let paused = tokio::spawn(async move {
        refresh_verified_event(
            &refresh_pool,
            &mut paused_client,
            &refresh_context,
            &refresh_event,
            &refresh_allocation,
            CollectionLimits::default(),
        )
        .await
    });
    timeout(Duration::from_secs(5), entered_wait)
        .await
        .expect("first provider request entered");

    let mut prepare_new_source = pool.begin().await.expect("begin new source preparation");
    sotto_server::cloud_provider::prepare_verified_event(
        &mut prepare_new_source,
        &context,
        &second_event,
        &second_allocation,
        "manual-new-source-run",
    )
    .await
    .expect("register and prepare second source");
    prepare_new_source
        .commit()
        .await
        .expect("commit second source preparation");
    release.notify_waiters();

    let stale = timeout(Duration::from_secs(5), paused)
        .await
        .expect("paused refresh completes")
        .expect("paused refresh joins");
    assert!(matches!(
        stale,
        Err(ProviderRefreshError::Completion(
            sotto_server::cloud_provider::ProviderAdapterError::CollectionSuperseded
        ))
    ));

    let mut retry_client = ScriptedClient {
        action: ClientAction::Pages(vec![
            page(&context, &fixture.source_id),
            page(&context, &second_allocation.source_id),
        ]),
    };
    let retry = refresh_verified_event(
        &pool,
        &mut retry_client,
        &context,
        &second_event,
        &second_allocation,
        CollectionLimits::default(),
    )
    .await
    .expect("expanded source set refresh succeeds");
    assert_eq!(
        retry.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );
    let first_status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&fixture.event_id)
            .fetch_one(&pool)
            .await
            .expect("read stale event status");
    assert_eq!(first_status, "pending");
    let second_status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&second_event_id)
            .fetch_one(&pool)
            .await
            .expect("read new event status");
    assert_eq!(second_status, "applied");

    cleanup_event_and_allocation(&pool, &second_event_id, &second_allocation_id).await;
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn competing_refreshes_use_distinct_runs_and_only_newest_applies() {
    let Some(pool) = pool_or_skip(8).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let context = context();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut old_client = ScriptedClient {
        action: ClientAction::Block {
            entered: entered.clone(),
            release: release.clone(),
            page: page(&context, &fixture.source_id),
        },
    };
    let old_pool = pool.clone();
    let old_context = context.clone();
    let old_event = event.clone();
    let old_allocation = allocation.clone();
    let entered_wait = entered.notified();
    let old_task = tokio::spawn(async move {
        refresh_verified_event(
            &old_pool,
            &mut old_client,
            &old_context,
            &old_event,
            &old_allocation,
            CollectionLimits::default(),
        )
        .await
    });
    timeout(Duration::from_secs(5), entered_wait)
        .await
        .expect("old provider request entered");

    let mut new_client = ScriptedClient {
        action: ClientAction::Page(page(&context, &fixture.source_id)),
    };
    let new_pool = pool.clone();
    let new_context = context.clone();
    let new_event = event.clone();
    let new_allocation = allocation.clone();
    let new_task = tokio::spawn(async move {
        refresh_verified_event(
            &new_pool,
            &mut new_client,
            &new_context,
            &new_event,
            &new_allocation,
            CollectionLimits::default(),
        )
        .await
    });
    let newest = timeout(Duration::from_secs(5), new_task)
        .await
        .expect("new refresh completes")
        .expect("new refresh joins")
        .expect("new refresh applies");
    assert_eq!(
        newest.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );
    release.notify_waiters();
    let stale = timeout(Duration::from_secs(5), old_task)
        .await
        .expect("old refresh completes")
        .expect("old refresh joins");
    assert!(matches!(
        stale,
        Err(ProviderRefreshError::Completion(
            sotto_server::cloud_provider::ProviderAdapterError::CollectionSuperseded
                | sotto_server::cloud_provider::ProviderAdapterError::EventNotPending
        ))
    ));
    let attempt_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_coverage_collection_attempts WHERE beneficiary_id = $1",
    )
    .bind(&fixture.beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("read competing attempts");
    assert_eq!(attempt_count, 2);

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn blocked_provider_fetch_releases_a_one_connection_pool() {
    let Some(pool) = pool_or_skip(1).await else {
        return;
    };
    let (fixture, event, allocation) = fixture(&pool).await;
    record_event(&pool, &event).await;
    let context = context();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut client = ScriptedClient {
        action: ClientAction::Block {
            entered: entered.clone(),
            release: release.clone(),
            page: page(&context, &fixture.source_id),
        },
    };
    let refresh_pool = pool.clone();
    let refresh_context = context.clone();
    let refresh_event = event.clone();
    let refresh_allocation = allocation.clone();
    let task = tokio::spawn(async move {
        refresh_verified_event(
            &refresh_pool,
            &mut client,
            &refresh_context,
            &refresh_event,
            &refresh_allocation,
            CollectionLimits {
                total_timeout: Duration::from_secs(5),
                request_timeout: Duration::from_secs(5),
                ..CollectionLimits::default()
            },
        )
        .await
    });
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("provider request entered");

    let query = timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&pool),
    )
    .await
    .expect("query is not blocked by provider fetch")
    .expect("query succeeds");
    assert_eq!(query, 1);
    release.notify_waiters();
    let receipt = timeout(Duration::from_secs(5), task)
        .await
        .expect("refresh task completes")
        .expect("refresh task joins")
        .expect("refresh succeeds");
    assert_eq!(
        receipt.outcome,
        sotto_server::cloud_provider::ApplyDisposition::Applied
    );

    cleanup(&pool, &fixture).await;
}
