//! Populated upgrade coverage for the scoped collection-attempt identity migration.
//!
//! This target is deliberately database-gated. It creates its own database so the pre-0025
//! schema can be populated without rewinding the database used by the other integration tests.

use std::borrow::Cow;
use std::str::FromStr;

use serde_json::Value;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use sotto_server::cloud_coverage::ConfirmedPaidInterval;
use sotto_server::cloud_coverage_reconciliation::{
    begin_collection, finish_collection, register_source, CollectionStatus, ReconciliationError,
    RegistrationOutcome, SourceBinding, SourceObservation,
};
use sotto_server::cloud_coverage_store::PublicationOutcome;
use sotto_server::cloud_provider::{
    replay_verified_event, AllocationState, PayerKind, ProviderContext, ProviderEnvironment,
    VerifiedAllocation, VerifiedCollection, VerifiedProviderEvent,
};
use sotto_server::db;

static ALL_MIGRATIONS: Migrator = sqlx::migrate!("./migrations");

struct DisposableDatabase {
    admin: PgPool,
    name: String,
    pool: PgPool,
}

impl DisposableDatabase {
    async fn create() -> Option<Self> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let base = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
        assert!(
            matches!(base.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing migration test against non-local host: {}",
            base.get_host()
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(base.clone().database("postgres"))
            .await
            .expect("connect to postgres maintenance database");
        let name = format!("sotto_cloud_upgrade_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .expect("create disposable migration database");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(base.database(&name))
            .await
            .expect("connect to disposable migration database");
        Some(Self { admin, name, pool })
    }

    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name))
            .execute(&self.admin)
            .await
            .expect("drop disposable migration database");
        self.admin.close().await;
    }
}

fn old_migrator() -> Migrator {
    migrator_before(25)
}

fn migrator_before(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            ALL_MIGRATIONS
                .iter()
                .filter(|migration| migration.version < version)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

async fn seed_legacy_provider_receipt(
    pool: &PgPool,
) -> (
    ProviderContext,
    VerifiedProviderEvent,
    VerifiedAllocation,
    VerifiedCollection,
) {
    let beneficiary = "provider-migration-beneficiary";
    let payer = "provider-migration-payer";
    let allocation_id = "provider-migration-allocation";
    let source_id = "provider-migration-source";
    let event_id = "provider-migration-event";
    let subscription_id = "provider-migration-subscription";
    let external_reference = "provider-migration-external";
    let context = ProviderContext::new(
        "legacy-provider",
        "legacy-account",
        ProviderEnvironment::Test,
    )
    .expect("provider context");
    let event = VerifiedProviderEvent::from_payload(
        event_id,
        "invoice.paid",
        1_700_000_000,
        Some(subscription_id.into()),
        Some(external_reference.into()),
        br#"{"status":"paid"}"#,
    )
    .expect("provider event");
    let allocation = VerifiedAllocation::new(
        allocation_id,
        payer,
        "provider-migration-customer",
        PayerKind::Personal,
        beneficiary,
        subscription_id,
        "provider-migration-item",
        external_reference,
        source_id,
        0,
        None,
        AllocationState::Active,
        "provider-migration-ownership",
    )
    .expect("provider allocation");
    let collection = VerifiedCollection {
        aggregate_evidence_reference: "legacy-aggregate".into(),
        observations: vec![SourceObservation::Complete {
            source_id: source_id.into(),
            evidence_reference: "legacy-evidence".into(),
            paid_intervals: vec![],
        }],
    };
    let provider_binding = SourceBinding {
        beneficiary_id: beneficiary.into(),
        source_id: source_id.into(),
        provider_namespace: context.namespace.clone(),
        external_allocation_reference: external_reference.into(),
        ownership_evidence_reference: "provider-migration-ownership".into(),
    };
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'migration-provider', $1)",
    )
    .bind(beneficiary)
    .execute(pool)
    .await
    .expect("insert provider beneficiary");
    sqlx::query("INSERT INTO cloud_coverage_revisions (beneficiary_id, revision, operation_id, evidence_reference, status, fact_count) VALUES ($1, 1, 'provider-migration-revision', 'provider-migration-revision-evidence', 'complete', 0)")
        .bind(beneficiary)
        .execute(pool)
        .await
        .expect("insert provider revision");
    sqlx::query(
        "INSERT INTO cloud_coverage_heads (beneficiary_id, current_revision) VALUES ($1, 1)",
    )
    .bind(beneficiary)
    .execute(pool)
    .await
    .expect("insert provider head");
    sqlx::query("INSERT INTO cloud_coverage_coordinators (beneficiary_id, source_set_generation, collection_epoch) VALUES ($1, 1, 1)")
        .bind(beneficiary)
        .execute(pool)
        .await
        .expect("insert provider coordinator");
    sqlx::query("INSERT INTO cloud_coverage_sources (source_id, beneficiary_id, provider_namespace, external_allocation_reference, ownership_evidence_reference, registration_operation_id, registration_source_set_generation, registration_projection_revision) VALUES ($1, $2, $3, $4, 'provider-migration-ownership', 'provider-migration-registration', 1, 1)")
        .bind(source_id)
        .bind(beneficiary)
        .bind(&context.namespace)
        .bind(external_reference)
        .execute(pool)
        .await
        .expect("insert provider source");
    sqlx::query("INSERT INTO cloud_provider_payers (payer_id, provider_namespace, provider_account_id, provider_environment, provider_customer_id, payer_kind) VALUES ($1, $2, $3, $4, $5, 'personal')")
        .bind(payer)
        .bind(&context.namespace)
        .bind(&context.account_id)
        .bind(context.environment.as_str())
        .bind("provider-migration-customer")
        .execute(pool)
        .await
        .expect("insert provider payer");
    sqlx::query("INSERT INTO cloud_provider_allocations (allocation_id, payer_id, beneficiary_id, provider_namespace, provider_account_id, provider_environment, provider_subscription_id, provider_item_id, external_allocation_reference, coverage_source_id, effective_from, state, ownership_evidence_reference) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 0, 'active', 'provider-migration-ownership')")
        .bind(allocation_id)
        .bind(payer)
        .bind(beneficiary)
        .bind(&context.namespace)
        .bind(&context.account_id)
        .bind(context.environment.as_str())
        .bind(subscription_id)
        .bind("provider-migration-item")
        .bind(external_reference)
        .bind(source_id)
        .execute(pool)
        .await
        .expect("insert provider allocation");
    let bindings = serde_json::to_string(&[&provider_binding]).expect("encode provider binding");
    sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, expected_projection_revision, source_bindings, status, aggregate_evidence_reference, canonical_result, projection_revision, completed_at) VALUES ($1, $2, 2, 1, 1, $3::jsonb, 'completed', 'legacy-aggregate', $4::jsonb, 1, now())")
        .bind(format!("provider-event:{event_id}"))
        .bind(beneficiary)
        .bind(bindings)
        .bind(canonical_result(
            &provider_binding,
            "legacy-aggregate",
            "legacy-evidence",
        ))
        .execute(pool)
        .await
        .expect("insert provider attempt");
    sqlx::query("INSERT INTO cloud_provider_event_receipts (provider_namespace, provider_account_id, provider_environment, event_id, event_type, provider_created_at, subscription_id, allocation_reference, normalized_payload_hash, status, allocation_id, coverage_source_id, projection_revision, processed_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'applied', $10, $11, 1, now())")
        .bind(&context.namespace)
        .bind(&context.account_id)
        .bind(context.environment.as_str())
        .bind(event_id)
        .bind(&event.event_type)
        .bind(event.provider_created_at)
        .bind(event.subscription_id.as_deref())
        .bind(event.allocation_reference.as_deref())
        .bind(&event.normalized_payload_hash)
        .bind(allocation_id)
        .bind(source_id)
        .execute(pool)
        .await
        .expect("insert provider receipt");
    (context, event, allocation, collection)
}

fn binding(beneficiary_id: &str, source_id: &str) -> SourceBinding {
    SourceBinding {
        beneficiary_id: beneficiary_id.into(),
        source_id: source_id.into(),
        provider_namespace: "legacy-provider".into(),
        external_allocation_reference: format!("allocation-{source_id}"),
        ownership_evidence_reference: format!("ownership-{source_id}"),
    }
}

fn canonical_result(binding: &SourceBinding, aggregate: &str, evidence: &str) -> Value {
    serde_json::json!({
        "aggregate_evidence_reference": aggregate,
        "sources": [{
            "source_id": binding.source_id,
            "evidence_reference": evidence,
            "status": "complete",
            "paid_intervals": [],
        }],
    })
}

async fn seed_legacy_database(pool: &PgPool) -> (String, String, SourceBinding, SourceBinding) {
    let first = "migration-beneficiary-a";
    let second = "migration-beneficiary-b";
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'migration-test', $1), ($2, 'migration-test', $2)",
    )
    .bind(first)
    .bind(second)
    .execute(pool)
    .await
    .expect("insert migration beneficiaries");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by, tier, trial_ends_at, stripe_customer_id, stripe_subscription_id) \
         VALUES ('migration-org', decode('6f7267', 'hex'), $1, 'team', now() + interval '3 days', 'cus_legacy', 'sub_legacy')",
    )
    .bind(first)
    .execute(pool)
    .await
    .expect("insert legacy organisation billing state");

    let first_binding = binding(first, "legacy-source-a");
    let second_binding = binding(second, "legacy-source-b");
    for (beneficiary, source, binding) in [
        (first, "legacy-source-a", &first_binding),
        (second, "legacy-source-b", &second_binding),
    ] {
        sqlx::query("INSERT INTO cloud_coverage_revisions (beneficiary_id, revision, operation_id, evidence_reference, status, fact_count) VALUES ($1, 1, $2, $3, 'complete', 1)")
            .bind(beneficiary)
            .bind(format!("legacy-revision-{beneficiary}"))
            .bind(format!("legacy-evidence-{source}"))
            .execute(pool)
            .await
            .expect("insert legacy complete revision");
        sqlx::query("INSERT INTO cloud_coverage_revision_facts (beneficiary_id, revision, coverage_id, source_id, starts_at, paid_until) VALUES ($1, 1, $2, $3, 10, 20)")
            .bind(beneficiary)
            .bind(format!("coverage-{source}"))
            .bind(source)
            .execute(pool)
            .await
            .expect("insert legacy coverage fact");
        sqlx::query("INSERT INTO cloud_coverage_revisions (beneficiary_id, revision, operation_id, evidence_reference, status, unavailable_reason, fact_count) VALUES ($1, 2, $2, $3, 'unavailable', 'needs_reconciliation', 0)")
            .bind(beneficiary)
            .bind(format!("legacy-unavailable-{beneficiary}"))
            .bind(format!("legacy-unavailable-evidence-{source}"))
            .execute(pool)
            .await
            .expect("insert legacy unavailable revision");
        sqlx::query(
            "INSERT INTO cloud_coverage_heads (beneficiary_id, current_revision) VALUES ($1, 2)",
        )
        .bind(beneficiary)
        .execute(pool)
        .await
        .expect("insert legacy coverage head");
        sqlx::query("INSERT INTO cloud_coverage_coordinators (beneficiary_id, source_set_generation, collection_epoch) VALUES ($1, 1, $2)")
            .bind(beneficiary)
            .bind(if beneficiary == first { 3 } else { 1 })
            .execute(pool)
            .await
            .expect("insert legacy coordinator");
        sqlx::query("INSERT INTO cloud_coverage_sources (source_id, beneficiary_id, provider_namespace, external_allocation_reference, ownership_evidence_reference, registration_operation_id, registration_source_set_generation, registration_projection_revision) VALUES ($1, $2, $3, $4, $5, $6, 1, 1)")
            .bind(&binding.source_id)
            .bind(beneficiary)
            .bind(&binding.provider_namespace)
            .bind(&binding.external_allocation_reference)
            .bind(&binding.ownership_evidence_reference)
            .bind(format!("legacy-registration-{source}"))
            .execute(pool)
            .await
            .expect("insert legacy source");
    }

    let first_json = serde_json::to_string(&[&first_binding]).expect("encode first bindings");
    let second_json = serde_json::to_string(&[&second_binding]).expect("encode second bindings");
    sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, expected_projection_revision, source_bindings, status) VALUES ('legacy-pending-a', $1, 3, 1, 2, $2::jsonb, 'pending'), ('legacy-superseded-a', $1, 1, 1, 2, $2::jsonb, 'superseded')")
        .bind(first)
        .bind(&first_json)
        .execute(pool)
        .await
        .expect("insert legacy incomplete attempts");
    sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, expected_projection_revision, source_bindings, status, aggregate_evidence_reference, canonical_result, projection_revision, completed_at) VALUES ('legacy-completed-a', $1, 2, 1, 1, $2::jsonb, 'completed', 'legacy-aggregate-a', $3::jsonb, 1, now()), ('legacy-completed-b', $4, 1, 1, 1, $5::jsonb, 'completed', 'legacy-aggregate-b', $6::jsonb, 1, now())")
        .bind(first)
        .bind(&first_json)
        .bind(canonical_result(&first_binding, "legacy-aggregate-a", "legacy-evidence-a"))
        .bind(second)
        .bind(&second_json)
        .bind(canonical_result(&second_binding, "legacy-aggregate-b", "legacy-evidence-b"))
        .execute(pool)
        .await
        .expect("insert legacy completed attempts");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = $2 WHERE beneficiary_id = $1",
    )
    .bind(first)
    .bind("legacy-pending-a")
    .execute(pool)
    .await
    .expect("link first legacy current attempt");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = $2 WHERE beneficiary_id = $1",
    )
    .bind(second)
    .bind("legacy-completed-b")
    .execute(pool)
    .await
    .expect("link second legacy current attempt");
    (first.into(), second.into(), first_binding, second_binding)
}

async fn snapshot(pool: &PgPool, query: &str) -> Vec<Value> {
    sqlx::query(query)
        .fetch_all(pool)
        .await
        .expect("read snapshot")
        .into_iter()
        .map(|row| row.try_get("value").expect("decode snapshot json"))
        .collect()
}

async fn coverage_snapshot(pool: &PgPool) -> Vec<Vec<Value>> {
    let queries = [
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_coordinators ORDER BY beneficiary_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_sources ORDER BY source_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_collection_attempts ORDER BY beneficiary_id, attempt_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_revisions ORDER BY beneficiary_id, revision) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_revision_facts ORDER BY beneficiary_id, revision, coverage_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_heads ORDER BY beneficiary_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT id, oauth_provider, oauth_subject, email FROM users WHERE id LIKE 'migration-beneficiary-%' ORDER BY id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT id, tier, trial_ends_at, stripe_customer_id, stripe_subscription_id FROM organizations WHERE id = 'migration-org') t",
    ];
    let mut snapshots = Vec::with_capacity(queries.len());
    for query in queries {
        snapshots.push(snapshot(pool, query).await);
    }
    snapshots
}

#[tokio::test]
async fn populated_0024_upgrade_preserves_coverage_and_scopes_attempt_identity() {
    let Some(database) = DisposableDatabase::create().await else {
        return;
    };
    let old = old_migrator();
    old.run(&database.pool)
        .await
        .expect("apply migrations through 0024");
    let (first, second, first_binding, second_binding) = seed_legacy_database(&database.pool).await;
    let before = coverage_snapshot(&database.pool).await;

    db::migrate(&database.pool)
        .await
        .expect("apply migration 0025");
    assert_eq!(coverage_snapshot(&database.pool).await, before);
    db::migrate(&database.pool)
        .await
        .expect("rerun full migrator");
    assert_eq!(coverage_snapshot(&database.pool).await, before);

    let mut tx = database.pool.begin().await.expect("begin pending load");
    let pending = begin_collection(&mut tx, &first, "legacy-pending-a")
        .await
        .expect("load legacy pending attempt");
    tx.commit().await.expect("commit pending load");
    assert_eq!(pending.status, CollectionStatus::Pending);

    let observation = SourceObservation::Complete {
        source_id: first_binding.source_id.clone(),
        evidence_reference: "new-source-evidence".into(),
        paid_intervals: vec![ConfirmedPaidInterval {
            coverage_id: "new-coverage".into(),
            source_id: first_binding.source_id.clone(),
            starts_at: 30,
            paid_until: 40,
            failed_renewal_id: None,
        }],
    };
    let mut tx = database.pool.begin().await.expect("begin pending finish");
    finish_collection(&mut tx, &pending, "new-aggregate-evidence", &[observation])
        .await
        .expect("finish upgraded pending attempt");
    tx.commit().await.expect("commit pending finish");

    let mut tx = database.pool.begin().await.expect("begin superseded load");
    let superseded = begin_collection(&mut tx, &first, "legacy-superseded-a")
        .await
        .expect("load legacy superseded attempt");
    tx.commit().await.expect("commit superseded load");
    assert_eq!(superseded.status, CollectionStatus::Superseded);
    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin superseded finish");
    let result = finish_collection(&mut tx, &superseded, "unused", &[]).await;
    tx.rollback().await.expect("rollback superseded finish");
    assert!(matches!(
        result,
        Err(ReconciliationError::AttemptSuperseded)
    ));

    let mut tx = database.pool.begin().await.expect("begin completed replay");
    let completed = begin_collection(&mut tx, &first, "legacy-completed-a")
        .await
        .expect("load legacy completed attempt");
    tx.commit().await.expect("commit completed load");
    let replay_observation = SourceObservation::Complete {
        source_id: first_binding.source_id.clone(),
        evidence_reference: "legacy-evidence-a".into(),
        paid_intervals: vec![],
    };
    let head_before_replay: i64 = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(&first)
    .fetch_one(&database.pool)
    .await
    .expect("read head before completed replay");
    let revisions_before_replay: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_coverage_revisions WHERE beneficiary_id = $1",
    )
    .bind(&first)
    .fetch_one(&database.pool)
    .await
    .expect("count revisions before completed replay");
    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin completed replay finish");
    let replay = finish_collection(
        &mut tx,
        &completed,
        "legacy-aggregate-a",
        &[replay_observation],
    )
    .await
    .expect("replay upgraded completed attempt");
    tx.commit().await.expect("commit completed replay");
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
        )
        .bind(&first)
        .fetch_one(&database.pool)
        .await
        .expect("read head after completed replay"),
        head_before_replay
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM cloud_coverage_revisions WHERE beneficiary_id = $1",
        )
        .bind(&first)
        .fetch_one(&database.pool)
        .await
        .expect("count revisions after completed replay"),
        revisions_before_replay
    );

    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin second completed replay");
    let second_completed = begin_collection(&mut tx, &second, "legacy-completed-b")
        .await
        .expect("load second beneficiary attempt");
    tx.commit().await.expect("commit second completed load");
    assert_eq!(second_completed.status, CollectionStatus::Completed);

    sqlx::query("INSERT INTO cloud_coverage_revisions (beneficiary_id, revision, operation_id, evidence_reference, status, unavailable_reason, fact_count) VALUES ($1, 4, 'second-only-revision', 'second-only-evidence', 'unavailable', 'needs_reconciliation', 0)")
        .bind(&second)
        .execute(&database.pool)
        .await
        .expect("insert second-only revision for foreign key checks");
    let first_source_json = serde_json::to_string(&[&first_binding]).expect("encode first source");

    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin foreign current attempt");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = $2 WHERE beneficiary_id = $1",
    )
    .bind(&first)
    .bind("legacy-completed-b")
    .execute(&mut *tx)
    .await
    .expect("write foreign current attempt");
    assert!(
        tx.commit().await.is_err(),
        "foreign current attempt committed"
    );

    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin foreign expected revision");
    sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, expected_projection_revision, source_bindings, status) VALUES ('foreign-expected-revision', $1, 10, 1, 4, $2::jsonb, 'pending')")
        .bind(&first)
        .bind(&first_source_json)
        .execute(&mut *tx)
        .await
        .expect("write foreign expected revision");
    assert!(
        tx.commit().await.is_err(),
        "foreign expected revision committed"
    );

    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin foreign completed revision");
    sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, source_bindings, status, aggregate_evidence_reference, canonical_result, projection_revision, completed_at) VALUES ('foreign-completed-revision', $1, 11, 1, $2::jsonb, 'completed', 'foreign-evidence', $3::jsonb, 4, now())")
        .bind(&first)
        .bind(&first_source_json)
        .bind(canonical_result(
            &first_binding,
            "foreign-evidence",
            "source-evidence",
        ))
        .execute(&mut *tx)
        .await
        .expect("write foreign completed revision");
    assert!(
        tx.commit().await.is_err(),
        "foreign completed revision committed"
    );

    let mut tx = database.pool.begin().await.expect("begin duplicate epoch");
    let duplicate_epoch = sqlx::query("INSERT INTO cloud_coverage_collection_attempts (attempt_id, beneficiary_id, collection_epoch, source_set_generation, source_bindings, status) VALUES ('duplicate-epoch', $1, 1, 1, $2::jsonb, 'pending')")
        .bind(&first)
        .bind(&first_source_json)
        .execute(&mut *tx)
        .await;
    assert!(
        duplicate_epoch.is_err(),
        "duplicate beneficiary epoch committed"
    );
    tx.rollback().await.expect("rollback duplicate epoch");

    for beneficiary in [&first, &second] {
        let mut tx = database
            .pool
            .begin()
            .await
            .expect("begin scoped identity operation");
        let ticket = begin_collection(&mut tx, beneficiary, "same-textual-id")
            .await
            .expect("public collection operation accepts scoped identity");
        tx.commit().await.expect("commit scoped identity operation");
        let mut tx = database
            .pool
            .begin()
            .await
            .expect("begin scoped identity completion");
        finish_collection(
            &mut tx,
            &ticket,
            "scoped-identity-aggregate",
            &[SourceObservation::Complete {
                source_id: if beneficiary == &first {
                    first_binding.source_id.clone()
                } else {
                    second_binding.source_id.clone()
                },
                evidence_reference: "scoped-identity-evidence".into(),
                paid_intervals: vec![],
            }],
        )
        .await
        .expect("public scoped identity completion");
        tx.commit()
            .await
            .expect("commit scoped identity completion");
    }

    let legacy_only = DisposableDatabase::create()
        .await
        .expect("create pre migration identity database");
    old.run(&legacy_only.pool)
        .await
        .expect("apply legacy migrations for identity check");
    let (legacy_first, legacy_second, _, _) = seed_legacy_database(&legacy_only.pool).await;
    let mut tx = legacy_only
        .pool
        .begin()
        .await
        .expect("begin legacy first identity");
    begin_collection(&mut tx, &legacy_first, "same-textual-id")
        .await
        .expect("legacy first identity operation");
    tx.commit().await.expect("commit legacy first identity");
    let mut tx = legacy_only
        .pool
        .begin()
        .await
        .expect("begin legacy second identity");
    let duplicate = begin_collection(&mut tx, &legacy_second, "same-textual-id").await;
    assert!(
        duplicate.is_err(),
        "pre migration schema must reject a cross beneficiary textual attempt id"
    );
    tx.rollback()
        .await
        .expect("rollback legacy duplicate identity");
    legacy_only.cleanup().await;

    let fresh = DisposableDatabase::create()
        .await
        .expect("create fresh migration database");
    db::migrate(&fresh.pool)
        .await
        .expect("migrate fresh database");
    for beneficiary in ["fresh-beneficiary-a", "fresh-beneficiary-b"] {
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'migration-test', $1)",
        )
        .bind(beneficiary)
        .execute(&fresh.pool)
        .await
        .expect("insert fresh beneficiary");
        let source = binding(beneficiary, &format!("{beneficiary}:source"));
        let mut tx = fresh.pool.begin().await.expect("begin fresh registration");
        let receipt = register_source(&mut tx, "fresh-registration", &source)
            .await
            .expect("register fresh source");
        tx.commit().await.expect("commit fresh registration");
        assert_eq!(receipt.outcome, RegistrationOutcome::Applied);
        let mut tx = fresh.pool.begin().await.expect("begin fresh collection");
        let ticket = begin_collection(&mut tx, beneficiary, "same-attempt")
            .await
            .expect("begin same attempt on fresh beneficiary");
        tx.commit().await.expect("commit fresh collection");
        let mut tx = fresh.pool.begin().await.expect("begin fresh finish");
        finish_collection(
            &mut tx,
            &ticket,
            "fresh-aggregate",
            &[SourceObservation::Complete {
                source_id: source.source_id,
                evidence_reference: "fresh-source-evidence".into(),
                paid_intervals: vec![],
            }],
        )
        .await
        .expect("finish same attempt on fresh beneficiary");
        tx.commit().await.expect("commit fresh finish");
    }
    fresh.cleanup().await;

    database.cleanup().await;
}

#[tokio::test]
async fn migration_0028_preserves_legacy_provider_replay() {
    let Some(database) = DisposableDatabase::create().await else {
        return;
    };
    migrator_before(27)
        .run(&database.pool)
        .await
        .expect("apply migrations through 0026");
    let (context, event, allocation, collection) =
        seed_legacy_provider_receipt(&database.pool).await;

    db::migrate(&database.pool)
        .await
        .expect("apply migration 0027 and legacy backfill");
    let association: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT collection_beneficiary_id, collection_attempt_id, collection_run_id \
         FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&event.event_id)
    .fetch_one(&database.pool)
    .await
    .expect("read backfilled receipt association");
    assert_eq!(
        association,
        (
            Some(allocation.beneficiary_id.clone()),
            Some(format!("provider-event:{}", event.event_id)),
            Some("legacy-provider-event-v1".into()),
        )
    );

    let mut tx = database.pool.begin().await.expect("begin legacy replay");
    let replay = replay_verified_event(&mut tx, &context, &event, &allocation, &collection)
        .await
        .expect("replay backfilled legacy receipt");
    tx.commit().await.expect("commit legacy replay");
    assert_eq!(
        replay.outcome,
        sotto_server::cloud_provider::ApplyDisposition::AlreadyApplied
    );

    sqlx::query(
        "UPDATE cloud_provider_event_receipts SET collection_beneficiary_id = NULL, \
                collection_attempt_id = NULL, collection_run_id = NULL WHERE event_id = $1",
    )
    .bind(&event.event_id)
    .execute(&database.pool)
    .await
    .expect("clear legacy association");
    let mut tx = database
        .pool
        .begin()
        .await
        .expect("begin null-association replay");
    let replay = replay_verified_event(&mut tx, &context, &event, &allocation, &collection)
        .await
        .expect("replay unassociated legacy receipt");
    tx.commit().await.expect("commit null-association replay");
    assert_eq!(
        replay.outcome,
        sotto_server::cloud_provider::ApplyDisposition::AlreadyApplied
    );
    database.cleanup().await;
}
