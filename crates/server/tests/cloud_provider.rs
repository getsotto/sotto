//! Provider adapter persistence and reconciliation acceptance.
//!
//! These tests are opt-in because they exercise the real Postgres schema. The unit tests in the
//! module cover validation without a database; this file proves the durable idempotency boundary
//! and the atomic apply path against the migrations.

use std::str::FromStr;

use sotto_server::cloud_coverage::ConfirmedPaidInterval;
use sotto_server::cloud_coverage_reconciliation::SourceObservation;
use sotto_server::cloud_provider::{
    complete_verified_event, prepare_verified_event, record_verified_event, reject_verified_event,
    replay_verified_event, AllocationState, ApplyDisposition, EventDisposition, PayerKind,
    ProviderContext, ProviderEnvironment, RejectionDisposition, VerifiedAllocation,
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

fn live_context() -> ProviderContext {
    ProviderContext::new("stripe", "acct_live_sotto", ProviderEnvironment::Live).unwrap()
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

async fn prepare_and_complete(
    pool: &PgPool,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    collection: &VerifiedCollection,
    run_id: &str,
) -> sotto_server::cloud_provider::ApplyReceipt {
    let mut prepare = pool.begin().await.unwrap();
    let preparation = prepare_verified_event(&mut prepare, context, event, allocation, run_id)
        .await
        .unwrap();
    prepare.commit().await.unwrap();
    let mut complete = pool.begin().await.unwrap();
    let receipt = complete_verified_event(
        &mut complete,
        context,
        event,
        allocation,
        &preparation,
        collection,
    )
    .await
    .unwrap();
    complete.commit().await.unwrap();
    receipt
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

    let rejected_event_id = format!("rejected-{}", Uuid::new_v4());
    let rejected_event = event(&rejected_event_id);
    let mut record_rejected = pool.begin().await.unwrap();
    record_verified_event(&mut record_rejected, &context, &rejected_event)
        .await
        .unwrap();
    record_rejected.commit().await.unwrap();
    let mut reject = pool.begin().await.unwrap();
    assert_eq!(
        reject_verified_event(&mut reject, &context, &rejected_event, "unsupported_event")
            .await
            .unwrap(),
        RejectionDisposition::Rejected
    );
    reject.commit().await.unwrap();
    let mut reject_replay = pool.begin().await.unwrap();
    assert_eq!(
        reject_verified_event(
            &mut reject_replay,
            &context,
            &rejected_event,
            "unsupported_event",
        )
        .await
        .unwrap(),
        RejectionDisposition::AlreadyRejected
    );
    reject_replay.commit().await.unwrap();

    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&original.event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&rejected_event_id)
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

    let first = prepare_and_complete(
        &pool,
        &context,
        &event,
        &allocation,
        &collection,
        "initial-run",
    )
    .await;
    assert_eq!(first.outcome, ApplyDisposition::Applied);
    assert!(first.revision > 0);

    let mut record_applied = pool.begin().await.unwrap();
    assert_eq!(
        record_verified_event(&mut record_applied, &context, &event)
            .await
            .unwrap(),
        EventDisposition::AlreadyApplied
    );
    record_applied.commit().await.unwrap();

    let renewal_event_id = format!("renewal-{suffix}");
    let renewal_event = VerifiedProviderEvent::from_payload(
        &renewal_event_id,
        "invoice.paid",
        1_700_000_001,
        Some(subscription_id.clone()),
        Some(external_reference.clone()),
        br#"{"status":"paid","renewal":true}"#,
    )
    .unwrap();
    let mut record_renewal = pool.begin().await.unwrap();
    record_verified_event(&mut record_renewal, &context, &renewal_event)
        .await
        .unwrap();
    record_renewal.commit().await.unwrap();
    let renewal = prepare_and_complete(
        &pool,
        &context,
        &renewal_event,
        &allocation,
        &collection,
        "renewal-run",
    )
    .await;
    assert_eq!(renewal.outcome, ApplyDisposition::Applied);
    assert!(renewal.revision > first.revision);

    let mut changed_allocation = allocation.clone();
    changed_allocation.provider_item_id = "different_item".into();
    let mut allocation_conflict = pool.begin().await.unwrap();
    let allocation_result = replay_verified_event(
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
    let collection_result = replay_verified_event(
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
    let second = replay_verified_event(&mut replay, &context, &event, &allocation, &collection)
        .await
        .unwrap();
    replay.commit().await.unwrap();
    assert_eq!(second.outcome, ApplyDisposition::AlreadyApplied);
    assert_eq!(second.revision, first.revision);

    let second_source_id = format!("second-source-{suffix}");
    let second_allocation_id = format!("second-allocation-{suffix}");
    let second_event_id = format!("second-event-{suffix}");
    let second_subscription_id = format!("second-subscription-{suffix}");
    let second_external_reference = format!("second-external-{suffix}");
    let second_allocation = VerifiedAllocation::new(
        &second_allocation_id,
        &payer_id,
        format!("cus-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &second_subscription_id,
        "price_cloud",
        &second_external_reference,
        &second_source_id,
        0,
        None,
        AllocationState::Active,
        format!("second-ownership-{suffix}"),
    )
    .unwrap();
    let second_event = VerifiedProviderEvent::from_payload(
        &second_event_id,
        "invoice.paid",
        1_700_000_002,
        Some(second_subscription_id),
        Some(second_external_reference),
        br#"{"status":"paid","source":2}"#,
    )
    .unwrap();
    let mut record_second = pool.begin().await.unwrap();
    record_verified_event(&mut record_second, &context, &second_event)
        .await
        .unwrap();
    record_second.commit().await.unwrap();

    let incomplete = VerifiedCollection {
        aggregate_evidence_reference: format!("incomplete-{suffix}"),
        observations: vec![SourceObservation::Complete {
            source_id: second_source_id.clone(),
            evidence_reference: format!("second-evidence-{suffix}"),
            paid_intervals: vec![ConfirmedPaidInterval {
                coverage_id: format!("second-coverage-{suffix}"),
                source_id: second_source_id.clone(),
                starts_at: 0,
                paid_until: 100,
                failed_renewal_id: None,
            }],
        }],
    };
    let mut incomplete_prepare = pool.begin().await.unwrap();
    let incomplete_preparation = prepare_verified_event(
        &mut incomplete_prepare,
        &context,
        &second_event,
        &second_allocation,
        "second-run",
    )
    .await
    .unwrap();
    incomplete_prepare.commit().await.unwrap();
    let mut incomplete_apply = pool.begin().await.unwrap();
    let incomplete_result = complete_verified_event(
        &mut incomplete_apply,
        &context,
        &second_event,
        &second_allocation,
        &incomplete_preparation,
        &incomplete,
    )
    .await;
    assert!(matches!(
        incomplete_result,
        Err(sotto_server::cloud_provider::ProviderAdapterError::IncompleteCollection)
    ));
    incomplete_apply.rollback().await.unwrap();
    let pending_status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(&second_event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending_status, "pending");
    let allocation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_allocations WHERE allocation_id = $1",
    )
    .bind(&second_allocation_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allocation_count, 1);
    let source_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_coverage_sources WHERE source_id = $1")
            .bind(&second_source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(source_count, 1);

    let complete = VerifiedCollection {
        aggregate_evidence_reference: format!("second-aggregate-{suffix}"),
        observations: vec![
            collection.observations[0].clone(),
            incomplete.observations[0].clone(),
        ],
    };
    let second_source = prepare_and_complete(
        &pool,
        &context,
        &second_event,
        &second_allocation,
        &complete,
        "second-run-retry",
    )
    .await;
    assert_eq!(second_source.outcome, ApplyDisposition::Applied);

    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&renewal_event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&second_event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&allocation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&second_allocation_id)
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

#[tokio::test]
async fn preparation_rejects_sources_from_another_provider_context() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let test_context = context();
    let live_context = live_context();
    let suffix = Uuid::new_v4().to_string();
    let beneficiary_id = format!("provider-context-test-{suffix}");
    let test_payer_id = format!("provider-context-test-payer-{suffix}");
    let live_payer_id = format!("provider-context-live-payer-{suffix}");
    let test_allocation_id = format!("provider-context-test-allocation-{suffix}");
    let live_allocation_id = format!("provider-context-live-allocation-{suffix}");
    let test_source_id = format!("provider-context-test-source-{suffix}");
    let live_source_id = format!("provider-context-live-source-{suffix}");
    let test_event_id = format!("provider-context-test-event-{suffix}");
    let live_event_id = format!("provider-context-live-event-{suffix}");
    let test_subscription = format!("provider-context-test-subscription-{suffix}");
    let live_subscription = format!("provider-context-live-subscription-{suffix}");
    let test_external = format!("provider-context-test-external-{suffix}");
    let live_external = format!("provider-context-live-external-{suffix}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'cloud-provider-test', $1)",
    )
    .bind(&beneficiary_id)
    .execute(&pool)
    .await
    .unwrap();
    let test_event = VerifiedProviderEvent::from_payload(
        &test_event_id,
        "invoice.paid",
        1_700_000_200,
        Some(test_subscription.clone()),
        Some(test_external.clone()),
        br#"{"status":"paid","environment":"test"}"#,
    )
    .unwrap();
    let live_event = VerifiedProviderEvent::from_payload(
        &live_event_id,
        "invoice.paid",
        1_700_000_201,
        Some(live_subscription.clone()),
        Some(live_external.clone()),
        br#"{"status":"paid","environment":"live"}"#,
    )
    .unwrap();
    let test_allocation = VerifiedAllocation::new(
        &test_allocation_id,
        &test_payer_id,
        format!("provider-context-test-customer-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &test_subscription,
        "price_cloud",
        &test_external,
        &test_source_id,
        0,
        None,
        AllocationState::Active,
        format!("provider-context-test-ownership-{suffix}"),
    )
    .unwrap();
    let live_allocation = VerifiedAllocation::new(
        &live_allocation_id,
        &live_payer_id,
        format!("provider-context-live-customer-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &live_subscription,
        "price_cloud",
        &live_external,
        &live_source_id,
        0,
        None,
        AllocationState::Active,
        format!("provider-context-live-ownership-{suffix}"),
    )
    .unwrap();
    for (context, event) in [(&test_context, &test_event), (&live_context, &live_event)] {
        let mut tx = pool.begin().await.unwrap();
        record_verified_event(&mut tx, context, event)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut prepare_test = pool.begin().await.unwrap();
    prepare_verified_event(
        &mut prepare_test,
        &test_context,
        &test_event,
        &test_allocation,
        "test-run",
    )
    .await
    .unwrap();
    prepare_test.commit().await.unwrap();

    let mut prepare_live = pool.begin().await.unwrap();
    let result = prepare_verified_event(
        &mut prepare_live,
        &live_context,
        &live_event,
        &live_allocation,
        "live-run",
    )
    .await;
    assert!(matches!(
        result,
        Err(sotto_server::cloud_provider::ProviderAdapterError::ProviderContextMismatch)
    ));
    prepare_live.rollback().await.unwrap();

    for event_id in [&test_event_id, &live_event_id] {
        sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
            .bind(event_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&test_allocation_id)
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
        .bind(&test_payer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn prepared_attempt_is_completed_after_external_collection() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let context = context();
    let suffix = Uuid::new_v4().to_string();
    let beneficiary_id = format!("prepared-provider-test-{suffix}");
    let payer_id = format!("prepared-payer-{suffix}");
    let allocation_id = format!("prepared-allocation-{suffix}");
    let source_id = format!("prepared-source-{suffix}");
    let event_id = format!("prepared-event-{suffix}");
    let subscription_id = format!("prepared-subscription-{suffix}");
    let external_reference = format!("prepared-external-{suffix}");
    let second_payer_id = format!("prepared-second-payer-{suffix}");
    let second_allocation_id = format!("prepared-second-allocation-{suffix}");
    let second_source_id = format!("prepared-second-source-{suffix}");
    let second_event_id = format!("prepared-second-event-{suffix}");
    let second_subscription_id = format!("prepared-second-subscription-{suffix}");
    let second_external_reference = format!("prepared-second-external-{suffix}");
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
        1_700_000_100,
        Some(subscription_id.clone()),
        Some(external_reference.clone()),
        br#"{"status":"paid"}"#,
    )
    .unwrap();
    let allocation = VerifiedAllocation::new(
        &allocation_id,
        &payer_id,
        format!("prepared-customer-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &subscription_id,
        "price_cloud",
        &external_reference,
        &source_id,
        0,
        None,
        AllocationState::Active,
        format!("prepared-ownership-{suffix}"),
    )
    .unwrap();
    let collection = VerifiedCollection {
        aggregate_evidence_reference: format!("prepared-aggregate-{suffix}"),
        observations: vec![SourceObservation::Complete {
            source_id: source_id.clone(),
            evidence_reference: format!("prepared-evidence-{suffix}"),
            paid_intervals: vec![ConfirmedPaidInterval {
                coverage_id: format!("prepared-coverage-{suffix}"),
                source_id: source_id.clone(),
                starts_at: 0,
                paid_until: 100,
                failed_renewal_id: None,
            }],
        }],
    };
    let second_event = VerifiedProviderEvent::from_payload(
        &second_event_id,
        "invoice.paid",
        1_700_000_101,
        Some(second_subscription_id.clone()),
        Some(second_external_reference.clone()),
        br#"{"status":"paid","source":2}"#,
    )
    .unwrap();
    let second_allocation = VerifiedAllocation::new(
        &second_allocation_id,
        &second_payer_id,
        format!("prepared-second-customer-{suffix}"),
        PayerKind::Personal,
        &beneficiary_id,
        &second_subscription_id,
        "price_cloud",
        &second_external_reference,
        &second_source_id,
        0,
        None,
        AllocationState::Active,
        format!("prepared-second-ownership-{suffix}"),
    )
    .unwrap();
    let second_collection = VerifiedCollection {
        aggregate_evidence_reference: format!("prepared-second-aggregate-{suffix}"),
        observations: vec![
            collection.observations[0].clone(),
            SourceObservation::Complete {
                source_id: second_source_id.clone(),
                evidence_reference: format!("prepared-second-evidence-{suffix}"),
                paid_intervals: vec![ConfirmedPaidInterval {
                    coverage_id: format!("prepared-second-coverage-{suffix}"),
                    source_id: second_source_id.clone(),
                    starts_at: 0,
                    paid_until: 100,
                    failed_renewal_id: None,
                }],
            },
        ],
    };
    let mut record = pool.begin().await.unwrap();
    record_verified_event(&mut record, &context, &event)
        .await
        .unwrap();
    record.commit().await.unwrap();
    let mut record_second = pool.begin().await.unwrap();
    record_verified_event(&mut record_second, &context, &second_event)
        .await
        .unwrap();
    record_second.commit().await.unwrap();

    let mut prepare = pool.begin().await.unwrap();
    let preparation = prepare_verified_event(&mut prepare, &context, &event, &allocation, "run-1")
        .await
        .unwrap();
    prepare.commit().await.unwrap();
    assert_eq!(preparation.run_id, "run-1");

    let mut retry_prepare = pool.begin().await.unwrap();
    let retry_preparation =
        prepare_verified_event(&mut retry_prepare, &context, &event, &allocation, "run-1")
            .await
            .unwrap();
    retry_prepare.commit().await.unwrap();
    assert_eq!(retry_preparation, preparation);

    let mut superseding_prepare = pool.begin().await.unwrap();
    let superseding = prepare_verified_event(
        &mut superseding_prepare,
        &context,
        &second_event,
        &second_allocation,
        "run-2",
    )
    .await
    .unwrap();
    superseding_prepare.commit().await.unwrap();

    let mut stale_prepare = pool.begin().await.unwrap();
    let stale_preparation =
        prepare_verified_event(&mut stale_prepare, &context, &event, &allocation, "run-1").await;
    assert!(matches!(
        stale_preparation,
        Err(sotto_server::cloud_provider::ProviderAdapterError::CollectionSuperseded)
    ));
    stale_prepare.rollback().await.unwrap();

    let mut stale_complete = pool.begin().await.unwrap();
    let stale_result = complete_verified_event(
        &mut stale_complete,
        &context,
        &event,
        &allocation,
        &preparation,
        &collection,
    )
    .await;
    assert!(matches!(
        stale_result,
        Err(sotto_server::cloud_provider::ProviderAdapterError::CollectionSuperseded)
    ));
    stale_complete.rollback().await.unwrap();

    let mut complete = pool.begin().await.unwrap();
    let receipt = complete_verified_event(
        &mut complete,
        &context,
        &second_event,
        &second_allocation,
        &superseding,
        &second_collection,
    )
    .await
    .unwrap();
    complete.commit().await.unwrap();
    assert_eq!(receipt.outcome, ApplyDisposition::Applied);

    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&second_event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&allocation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&second_allocation_id)
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
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
        .bind(&second_payer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .unwrap();
}
