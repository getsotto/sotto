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
        _binding: &SourceBinding,
        _cursor: Option<&str>,
    ) -> Result<ProviderHistoryPage, ProviderCollectionError> {
        match &self.action {
            ClientAction::Page(page) => Ok(page.clone()),
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

fn page(context: &ProviderContext, source_id: &str) -> ProviderHistoryPage {
    ProviderHistoryPage {
        context: context.clone(),
        source_id: source_id.into(),
        evidence_reference: "provider-page-evidence".into(),
        paid_intervals: vec![ConfirmedPaidInterval {
            coverage_id: "provider-refresh-coverage".into(),
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
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&fixture.event_id)
        .execute(pool)
        .await
        .expect("delete fixture receipt");
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&fixture.allocation_id)
        .execute(pool)
        .await
        .expect("delete fixture allocation");
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
