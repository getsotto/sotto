//! Provider adapter persistence and reconciliation acceptance.
//!
//! These tests are opt-in because they exercise the real Postgres schema. The unit tests in the
//! module cover validation without a database; this file proves the durable idempotency boundary
//! and the atomic apply path against the migrations.

use std::str::FromStr;

use sotto_server::cloud_coverage::ConfirmedPaidInterval;
use sotto_server::cloud_coverage_reconciliation::SourceObservation;
use sotto_server::cloud_provider::{
    apply_verified_event, record_verified_event, AllocationState, ApplyDisposition,
    EventDisposition, PayerKind, ProviderContext, ProviderEnvironment, VerifiedAllocation,
    VerifiedCollection, VerifiedProviderEvent,
};
use sotto_server::db;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use uuid::Uuid;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping cloud provider test: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing cloud provider tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn context() -> ProviderContext {
    ProviderContext::new("stripe", "acct_test_sotto", ProviderEnvironment::Test).unwrap()
}

fn event(id: &str) -> VerifiedProviderEvent {
    VerifiedProviderEvent::from_payload(
        id,
        "invoice.paid",
        1_700_000_000,
        Some("sub_test_1".into()),
        Some("allocation_ref_1".into()),
        br#"{"amount":299,"status":"paid"}"#,
    )
    .unwrap()
}

#[tokio::test]
async fn verified_event_receipts_are_idempotent_and_conflicts_are_rejected() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let context = context();
    let original = event(&format!("evt-{}", Uuid::new_v4()));

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        record_verified_event(&mut tx, &context, &original)
            .await
            .unwrap(),
        EventDisposition::Pending
    );
    tx.commit().await.unwrap();

    let mut replay = pool.begin().await.unwrap();
    assert_eq!(
        record_verified_event(&mut replay, &context, &original)
            .await
            .unwrap(),
        EventDisposition::Pending
    );
    replay.commit().await.unwrap();

    let conflicting = VerifiedProviderEvent::from_payload(
        &original.event_id,
        &original.event_type,
        original.provider_created_at,
        original.subscription_id.clone(),
        original.allocation_reference.clone(),
        br#"{"amount":199,"status":"paid"}"#,
    )
    .unwrap();
    let mut conflict = pool.begin().await.unwrap();
    let result = record_verified_event(&mut conflict, &context, &conflicting).await;
    assert!(matches!(
        result,
        Err(sotto_server::cloud_provider::ProviderAdapterError::EventConflict)
    ));
    conflict.rollback().await.unwrap();

    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&original.event_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn applying_verified_event_commits_allocation_and_projection_once() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let context = context();
    let suffix = Uuid::new_v4().to_string();
    let beneficiary_id = format!("cloud-provider-test-{suffix}");
    let payer_id = format!("payer-{suffix}");
    let allocation_id = format!("allocation-{suffix}");
    let source_id = format!("source-{suffix}");
    let event_id = format!("evt-{suffix}");
    let subscription_id = format!("sub-{suffix}");
    let external_reference = format!("external-{suffix}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'cloud-provider-test', $1)",
    )
    .bind(&beneficiary_id)
    .execute(&pool)
    .await
    .unwrap();
    let event = VerifiedProviderEvent::from_payload(
        &event_id,
        "invoice.paid",
        1_700_000_000,
        Some(subscription_id.clone()),
        Some(external_reference.clone()),
        br#"{"status":"paid"}"#,
    )
    .unwrap();
    let allocation = VerifiedAllocation::new(
        &allocation_id,
        &payer_id,
        format!("cus-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &subscription_id,
        "price_cloud",
        &external_reference,
        &source_id,
        0,
        None,
        AllocationState::Active,
        format!("ownership-{suffix}"),
    )
    .unwrap();
    let collection = VerifiedCollection {
        aggregate_evidence_reference: format!("aggregate-{suffix}"),
        observations: vec![SourceObservation::Complete {
            source_id: source_id.clone(),
            evidence_reference: format!("evidence-{suffix}"),
            paid_intervals: vec![ConfirmedPaidInterval {
                coverage_id: format!("coverage-{suffix}"),
                source_id: source_id.clone(),
                starts_at: 0,
                paid_until: 100,
                failed_renewal_id: None,
            }],
        }],
    };

    let mut record = pool.begin().await.unwrap();
    record_verified_event(&mut record, &context, &event)
        .await
        .unwrap();
    record.commit().await.unwrap();

    let mut apply = pool.begin().await.unwrap();
    let first = apply_verified_event(&mut apply, &context, &event, &allocation, &collection)
        .await
        .unwrap();
    apply.commit().await.unwrap();
    assert_eq!(first.outcome, ApplyDisposition::Applied);
    assert!(first.revision > 0);

    let mut changed_allocation = allocation.clone();
    changed_allocation.provider_item_id = "different_item".into();
    let mut allocation_conflict = pool.begin().await.unwrap();
    let allocation_result = apply_verified_event(
        &mut allocation_conflict,
        &context,
        &event,
        &changed_allocation,
        &collection,
    )
    .await;
    assert!(matches!(
        allocation_result,
        Err(sotto_server::cloud_provider::ProviderAdapterError::AllocationConflict)
    ));
    allocation_conflict.rollback().await.unwrap();

    let mut changed_collection = collection.clone();
    changed_collection.aggregate_evidence_reference = format!("changed-{suffix}");
    let mut collection_conflict = pool.begin().await.unwrap();
    let collection_result = apply_verified_event(
        &mut collection_conflict,
        &context,
        &event,
        &allocation,
        &changed_collection,
    )
    .await;
    assert!(matches!(
        collection_result,
        Err(
            sotto_server::cloud_provider::ProviderAdapterError::Reconciliation(
                sotto_server::cloud_coverage_reconciliation::ReconciliationError::OperationConflict
            )
        )
    ));
    collection_conflict.rollback().await.unwrap();

    let mut replay = pool.begin().await.unwrap();
    let second = apply_verified_event(&mut replay, &context, &event, &allocation, &collection)
        .await
        .unwrap();
    replay.commit().await.unwrap();
    assert_eq!(second.outcome, ApplyDisposition::AlreadyApplied);
    assert_eq!(second.revision, first.revision);

    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&allocation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary_id)
    .execute(&pool)
    .await
    .unwrap();
    for table in [
        "cloud_coverage_collection_attempts",
        "cloud_coverage_sources",
        "cloud_coverage_coordinators",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&beneficiary_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE cloud_coverage_heads SET current_revision = NULL WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary_id)
    .execute(&pool)
    .await
    .unwrap();
    for table in ["cloud_coverage_revisions", "cloud_coverage_heads"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&beneficiary_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
        .bind(&payer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .unwrap();
}
