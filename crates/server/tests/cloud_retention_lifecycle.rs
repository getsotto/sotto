//! PostgreSQL coverage for the personal expiry candidate and non-destructive dry-run path.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use sha2::Digest;
use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tower::ServiceExt;

use sotto_server::cloud_coverage::{ConfirmedPaidInterval, EXPORT_WINDOW_SECONDS};
use sotto_server::cloud_coverage_reconciliation::{
    begin_collection, finish_collection_with_authority, register_source, AuthorityReceiptInput,
    SourceBinding, SourceObservation,
};
use sotto_server::config::DeploymentMode;
use sotto_server::db;
use sotto_server::retention::{self, EnqueueOutcome, OperatorAction, RetentionIntent, ScopeKind};
use sotto_server::state::AppState;

fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping cloud retention lifecycle tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing cloud retention lifecycle tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

fn state(pool: PgPool) -> AppState {
    AppState {
        pool,
        deployment_mode: DeploymentMode::Cloud,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: 30,
        organisation_deletion_metrics_token: None,
        cloud_operations_metrics_token: None,
        cloud_coverage_repair_token: None,
        provider_refresh_ingest_enabled: false,
        provider_refresh_worker_enabled: false,
        provider_refresh_reconciliation_enabled: false,
        provider_authority_revalidation_enabled: false,
        cloud_sales_enabled: false,
        founding_schedule_worker_enabled: false,
        founding_price_notices_enabled: false,
        billing_recovery_worker_enabled: false,
        sponsored_billing_recovery_worker_enabled: false,
        organisation_deletion_operator_token: None,
        billing_transfer_operator_token: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled: false,
        billing_transfer_execution_enabled: false,
        billing_transfer_setup_enabled: false,
        cloud_retention_dry_run_enabled: true,
        cloud_shared_retention_dry_run_enabled: true,
        cloud_retention_operator_token: Some("0123456789abcdef0123456789abcdef".into()),
        cloud_retention_purge_enabled: false,
        cloud_retention_destructive_token: None,
        cloud_retention_evidence_hmac_key: None,
        cloud_retention_build_sha: None,
    }
}

async fn seed_user(pool: &PgPool, user_id: &str) {
    sqlx::query(
        "INSERT INTO users(id,oauth_provider,oauth_subject) VALUES($1,'retention-test',$1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed user");
}

async fn seed_expired_projection(pool: &PgPool, user_id: &str, now: i64) -> (i64, i64) {
    let export_start = now - EXPORT_WINDOW_SECONDS - 120;
    let deadline = export_start + EXPORT_WINDOW_SECONDS;
    seed_authoritative_source(pool, user_id).await;
    publish_authoritative_coverage(
        pool,
        user_id,
        vec![ConfirmedPaidInterval {
            coverage_id: format!("coverage:{user_id}"),
            source_id: format!("source:{user_id}"),
            starts_at: export_start - 86_400,
            paid_until: export_start,
            failed_renewal_id: None,
        }],
        now,
        "expired",
    )
    .await;
    (export_start, deadline)
}

async fn seed_coverage_until(
    pool: &PgPool,
    user_id: &str,
    paid_until: i64,
    now: i64,
    suffix: &str,
) {
    seed_authoritative_source(pool, user_id).await;
    publish_authoritative_coverage(
        pool,
        user_id,
        vec![ConfirmedPaidInterval {
            coverage_id: format!("coverage:{user_id}"),
            source_id: format!("source:{user_id}"),
            starts_at: paid_until - 86_400,
            paid_until,
            failed_renewal_id: None,
        }],
        now,
        suffix,
    )
    .await;
}

async fn scan_shared_organisation(state: &AppState, organisation_id: &str) {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT max(id) FROM organizations WHERE id COLLATE \"C\" < $1 COLLATE \"C\"",
    )
    .bind(organisation_id)
    .fetch_one(&state.pool)
    .await
    .expect("find previous organisation for scoped scan");
    sqlx::query(
        "UPDATE cloud_retention_scan_state SET cursor_scope_id=$1,lease_owner=NULL,lease_expires_at=NULL \
         WHERE scan_key='shared_organisation_retention'",
    )
    .bind(previous)
    .execute(&state.pool)
    .await
    .expect("position the shared retention scan");
    sotto_server::cloud_shared_retention::reconcile_once(state)
        .await
        .expect("reconcile scoped shared organisation");
}

async fn scan_shared_organisation_until_job(state: &AppState, organisation_id: &str) {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT max(id) FROM organizations WHERE id COLLATE \"C\" < $1 COLLATE \"C\"",
    )
    .bind(organisation_id)
    .fetch_one(&state.pool)
    .await
    .expect("find previous organisation for scoped scan");
    sqlx::query(
        "UPDATE cloud_retention_scan_state SET cursor_scope_id=$1,lease_owner=NULL,lease_expires_at=NULL \
         WHERE scan_key='shared_organisation_retention'",
    )
    .bind(previous)
    .execute(&state.pool)
    .await
    .expect("position the shared retention scan");
    for _ in 0..32 {
        sotto_server::cloud_shared_retention::reconcile_once(state)
            .await
            .expect("reconcile shared organisations");
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation')",
        )
        .bind(organisation_id)
        .fetch_one(&state.pool)
        .await
        .expect("check for prepared shared candidate");
        if found {
            return;
        }
    }
    panic!("shared retention scanner did not reach organisation {organisation_id}");
}

async fn wait_for_shared_worker_blocked(pool: &PgPool, holder_pid: i32) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiter: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity \
             WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)) \
               AND query LIKE 'SELECT id FROM organizations WHERE id=$1 FOR UPDATE%' \
             LIMIT 1",
        )
        .bind(holder_pid)
        .fetch_optional(pool)
        .await
        .expect("inspect the shared retention lock wait");
        if let Some(waiter) = waiter {
            return waiter;
        }
        assert!(
            Instant::now() < deadline,
            "shared dry-run worker did not block on the organisation row"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn seed_authoritative_source(pool: &PgPool, user_id: &str) {
    seed_authoritative_source_for_payer(pool, user_id, &format!("payer:{user_id}"), "personal")
        .await;
}

async fn seed_sponsored_coverage_until(
    pool: &PgPool,
    user_id: &str,
    sponsoring_organisation_id: &str,
    paid_until: i64,
    now: i64,
) {
    seed_authoritative_source_for_payer(pool, user_id, sponsoring_organisation_id, "sponsor").await;
    publish_authoritative_coverage(
        pool,
        user_id,
        vec![ConfirmedPaidInterval {
            coverage_id: format!("sponsored-coverage:{user_id}"),
            source_id: format!("source:{user_id}"),
            starts_at: paid_until - 86_400,
            paid_until,
            failed_renewal_id: None,
        }],
        now,
        "sponsored-admin",
    )
    .await;
}

async fn seed_authoritative_source_for_payer(
    pool: &PgPool,
    user_id: &str,
    payer_id: &str,
    payer_kind: &str,
) {
    let source = SourceBinding {
        beneficiary_id: user_id.to_owned(),
        source_id: format!("source:{user_id}"),
        provider_namespace: "stripe".into(),
        external_allocation_reference: format!("allocation-reference:{user_id}"),
        ownership_evidence_reference: format!("ownership:{user_id}"),
    };
    let mut tx = pool.begin().await.expect("begin authority source setup");
    register_source(&mut tx, &format!("register:{user_id}"), &source)
        .await
        .expect("register authority source");
    sqlx::query(
        "INSERT INTO cloud_provider_payers \
         (payer_id, provider_namespace, provider_account_id, provider_environment, provider_customer_id, payer_kind) \
         VALUES ($1, 'stripe', 'acct_retention_test', 'test', $2, $3)",
    )
    .bind(payer_id)
    .bind(format!("customer:{user_id}"))
    .bind(payer_kind)
    .execute(&mut *tx)
    .await
    .expect("insert authority payer");
    sqlx::query(
        "INSERT INTO cloud_provider_allocations \
         (allocation_id, payer_id, beneficiary_id, provider_namespace, provider_account_id, \
          provider_environment, provider_subscription_id, provider_item_id, external_allocation_reference, \
          coverage_source_id, effective_from, effective_until, state, ownership_evidence_reference) \
         VALUES ($1, $2, $3, 'stripe', 'acct_retention_test', 'test', $4, $5, $6, $7, 0, $8, 'ended', $9)",
    )
    .bind(format!("allocation:{user_id}"))
    .bind(payer_id)
    .bind(user_id)
    .bind(format!("subscription:{user_id}"))
    .bind(format!("item:{user_id}"))
    .bind(&source.external_allocation_reference)
    .bind(&source.source_id)
    .bind(1_i64)
    .bind(&source.ownership_evidence_reference)
    .execute(&mut *tx)
    .await
    .expect("insert authority allocation");
    tx.commit().await.expect("commit authority source setup");
}

async fn publish_authoritative_coverage(
    pool: &PgPool,
    user_id: &str,
    paid_intervals: Vec<ConfirmedPaidInterval>,
    now: i64,
    suffix: &str,
) {
    let source_id = format!("source:{user_id}");
    let source_material = format!("{}:{}\nnone\n", source_id.len(), source_id);
    let correction_digest = format!("{:x}", sha2::Sha256::digest(source_material.as_bytes()));
    let attempt_id = format!("scheduled-revalidation:{user_id}:{suffix}");
    let mut tx = pool.begin().await.expect("begin authoritative collection");
    let ticket = begin_collection(&mut tx, user_id, &attempt_id)
        .await
        .expect("prepare authoritative collection");
    let authority = AuthorityReceiptInput {
        provider_namespace: "stripe".into(),
        provider_account_id: "acct_retention_test".into(),
        provider_environment: "test".into(),
        observed_at_epoch: now - 60,
        fresh_until_epoch: now + 90 * 24 * 60 * 60,
        policy_version: sotto_server::cloud_coverage_authority::POLICY_VERSION,
        local_correction_digest: correction_digest,
    };
    finish_collection_with_authority(
        &mut tx,
        &ticket,
        &format!("retention-evidence:{user_id}:{suffix}"),
        &[SourceObservation::Complete {
            source_id,
            evidence_reference: format!("provider-evidence:{user_id}:{suffix}"),
            paid_intervals,
        }],
        Some(&authority),
    )
    .await
    .expect("publish authoritative projection");
    tx.commit().await.expect("commit authoritative projection");
    assert!(matches!(
        sotto_server::cloud_coverage_authority::load_current(pool, user_id, now)
            .await
            .expect("load seeded authority"),
        sotto_server::cloud_coverage_authority::AuthorityState::Current { .. }
    ));
}

async fn seed_notice(pool: &PgPool, user_id: &str, start: i64, deadline: i64, created_at: i64) {
    let event_key = format!("cloud-export-window:{start}:{deadline}");
    let payload = serde_json::json!({
        "title": "Your Cloud account is in its export period",
        "detail": "Download an encrypted export before your export deadline.",
        "effective_at_epoch": start,
        "deadline_epoch": deadline,
        "amount_pence": null,
    });
    sqlx::query(
        "INSERT INTO cloud_notice_outbox \
         (notice_id,recipient_user_id,event_key,policy_key,kind,channel,payload,due_at,available_at,created_at) \
         VALUES($1,$2,$3,'cloud-export-deadline-v1','export_deadline','in_app',$4,to_timestamp($5),to_timestamp($6),to_timestamp($7))",
    )
    .bind(format!("notice:{user_id}"))
    .bind(user_id)
    .bind(event_key)
    .bind(payload)
    .bind(start)
    .bind(start)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("seed timely in-app notice");
}

async fn cleanup(pool: &PgPool, user_id: &str, organisation_id: &str) {
    sqlx::query(
        "DELETE FROM cloud_retention_purge_approvals WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("remove purge approvals");
    sqlx::query(
        "DELETE FROM cloud_retention_receipts WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("remove purge receipts");
    sqlx::query(
        "DELETE FROM cloud_retention_operator_actions WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("remove operator audit rows");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE scope_key=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove retention jobs");
    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove notices");
    sqlx::query("DELETE FROM cloud_coverage_authority_receipts WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove authority receipts");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL WHERE beneficiary_id=$1",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("clear current coverage attempt");
    sqlx::query("DELETE FROM cloud_coverage_collection_attempts WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove collection attempts");
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove provider allocations");
    sqlx::query("DELETE FROM cloud_coverage_source_retirements WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove source retirements");
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove coverage sources");
    sqlx::query("DELETE FROM cloud_coverage_revalidation_jobs WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove revalidation job");
    sqlx::query("DELETE FROM cloud_coverage_coordinators WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove coverage coordinator");
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove coverage head");
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove coverage facts");
    sqlx::query("DELETE FROM cloud_coverage_revisions WHERE beneficiary_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove coverage revisions");
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id=$1")
        .bind(format!("payer:{user_id}"))
        .execute(pool)
        .await
        .expect("remove provider payer");
    sqlx::query("DELETE FROM projects WHERE owner_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove projects");
    sqlx::query("DELETE FROM share_links WHERE created_by=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove share links");
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(organisation_id)
        .execute(pool)
        .await
        .expect("remove test organisation");
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("remove test user");
}

#[tokio::test]
async fn protected_retention_inventory_classifies_shared_resources_and_link_promises() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let operator_id = format!("retention-inventory-operator-{suffix}");
    let member_id = format!("retention-inventory-member-{suffix}");
    let owned_org_id = format!("retention-inventory-owned-org-{suffix}");
    let orphan_org_id = format!("retention-inventory-orphan-org-{suffix}");
    let project_id = format!("retention-inventory-project-{suffix}");
    let environment_id = format!("retention-inventory-environment-{suffix}");
    let active_free_id = format!("retention-inventory-share-free-{suffix}");
    let active_paid_id = format!("retention-inventory-share-paid-{suffix}");
    let active_legacy_id = format!("retention-inventory-share-legacy-{suffix}");
    let consumed_id = format!("retention-inventory-share-consumed-{suffix}");
    let revoked_id = format!("retention-inventory-share-revoked-{suffix}");
    let expired_id = format!("retention-inventory-share-expired-{suffix}");
    let multi_use_id = format!("retention-inventory-share-multi-use-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &operator_id, &owned_org_id).await;
    cleanup(&pool, &member_id, &orphan_org_id).await;
    seed_user(&pool, &operator_id).await;
    seed_user(&pool, &member_id).await;

    sqlx::query(
        "INSERT INTO organizations(id,enc_name,created_by) VALUES($1,decode('01','hex'),$2)",
    )
    .bind(&owned_org_id)
    .bind(&operator_id)
    .execute(&pool)
    .await
    .expect("seed owned organisation");
    sqlx::query("INSERT INTO organizations(id,enc_name) VALUES($1,decode('02','hex'))")
        .bind(&orphan_org_id)
        .execute(&pool)
        .await
        .expect("seed organisation without members");
    sqlx::query(
        "INSERT INTO organization_memberships(org_id,user_id,role) VALUES($1,$2,'owner'),($1,$3,'member')",
    )
    .bind(&owned_org_id)
    .bind(&operator_id)
    .bind(&member_id)
    .execute(&pool)
    .await
    .expect("seed organisation roles");
    sqlx::query(
        "INSERT INTO projects(id,owner_id,org_id,enc_name) VALUES($1,$2,$3,decode('03','hex'))",
    )
    .bind(&project_id)
    .bind(&operator_id)
    .bind(&owned_org_id)
    .execute(&pool)
    .await
    .expect("seed shared project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) VALUES($1,$2,decode('04','hex'),4)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed shared environment");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) VALUES($1,$2,decode('05','hex'))",
    )
    .bind(&environment_id)
    .bind(&member_id)
    .execute(&pool)
    .await
    .expect("seed shared environment grant");
    sqlx::query(
        "INSERT INTO share_links(id,token,enc_blob,created_by,max_views,view_count,expires_at,revoked_at,share_class) \
         VALUES($1,$2,decode('01','hex'),$3,1,0,to_timestamp($4),NULL,'free'), \
               ($5,$6,decode('02','hex'),$3,1,0,NULL,NULL,'paid'), \
               ($7,$8,decode('03','hex'),$3,1,0,NULL,NULL,'legacy'), \
               ($9,$10,decode('04','hex'),$3,1,1,to_timestamp($4),NULL,'free'), \
               ($11,$12,decode('05','hex'),$3,1,0,to_timestamp($4),to_timestamp($13),'paid'), \
               ($14,$15,decode('06','hex'),$3,1,0,to_timestamp($16),NULL,'legacy'), \
               ($17,$18,decode('07','hex'),$3,5,0,to_timestamp($19),NULL,'paid')",
    )
    .bind(&active_free_id)
    .bind(format!("retention-inventory-token-free-{suffix}"))
    .bind(&operator_id)
    .bind(now + 86_400)
    .bind(&active_paid_id)
    .bind(format!("retention-inventory-token-paid-{suffix}"))
    .bind(&active_legacy_id)
    .bind(format!("retention-inventory-token-legacy-{suffix}"))
    .bind(&consumed_id)
    .bind(format!("retention-inventory-token-consumed-{suffix}"))
    .bind(&revoked_id)
    .bind(format!("retention-inventory-token-revoked-{suffix}"))
    .bind(now - 100)
    .bind(&expired_id)
    .bind(format!("retention-inventory-token-expired-{suffix}"))
    .bind(now - 200)
    .bind(&multi_use_id)
    .bind(format!("retention-inventory-token-multi-use-{suffix}"))
    .bind(now + 86_400)
    .execute(&pool)
    .await
    .expect("seed share lifetime classes");

    let operator = sotto_server::retention_ops::router().with_state(state(pool.clone()));
    let unauthorised = operator
        .clone()
        .oneshot(
            Request::get("/ops/cloud/retention/inventory")
                .body(Body::empty())
                .expect("build unauthorised inventory request"),
        )
        .await
        .expect("unauthorised inventory response");
    assert_eq!(unauthorised.status(), StatusCode::UNAUTHORIZED);

    let response = operator
        .oneshot(
            Request::get("/ops/cloud/retention/inventory")
                .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
                .body(Body::empty())
                .expect("build authorised inventory request"),
        )
        .await
        .expect("authorised inventory response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read inventory response");
    let body_text = String::from_utf8(body.to_vec()).expect("inventory JSON is UTF-8");
    for private_value in [
        &operator_id,
        &member_id,
        &owned_org_id,
        &orphan_org_id,
        &project_id,
        &environment_id,
        &active_free_id,
        &active_paid_id,
        &active_legacy_id,
        &consumed_id,
        &revoked_id,
        &expired_id,
        &multi_use_id,
        "retention-inventory-token-",
    ] {
        assert!(!body_text.contains(private_value), "leaked {private_value}");
    }
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("parse inventory JSON");
    let organisations = body["organisations"]
        .as_array()
        .expect("organisation buckets");
    assert!(organisations.iter().any(|bucket| {
        bucket["lifecycle"] == "active"
            && bucket["authority"] == "owner_or_admin"
            && bucket["project_band"] == "one"
            && bucket["projects"].as_i64().is_some_and(|count| count >= 1)
            && bucket["environments"]
                .as_i64()
                .is_some_and(|count| count >= 1)
            && bucket["grants"].as_i64().is_some_and(|count| count >= 1)
    }));
    assert!(organisations.iter().any(|bucket| {
        bucket["lifecycle"] == "active"
            && bucket["authority"] == "no_members"
            && bucket["project_band"] == "none"
            && bucket["organisations"]
                .as_i64()
                .is_some_and(|count| count >= 1)
    }));
    let shares = body["shares"].as_array().expect("share buckets");
    for (class, lifetime, view_limit, share_state) in [
        ("free", "finite", "single_use", "active"),
        ("paid", "no_expiry", "single_use", "active"),
        ("legacy", "no_expiry", "single_use", "active"),
        ("free", "finite", "single_use", "consumed"),
        ("paid", "finite", "single_use", "revoked"),
        ("legacy", "finite", "single_use", "expired"),
        ("paid", "finite", "multi_use", "active"),
    ] {
        assert!(
            shares.iter().any(|bucket| {
                bucket["share_class"] == class
                    && bucket["lifetime"] == lifetime
                    && bucket["view_limit"] == view_limit
                    && bucket["state"] == share_state
                    && bucket["count"].as_i64().is_some_and(|count| count >= 1)
            }),
            "missing aggregate class {class}/{lifetime}/{view_limit}/{share_state}"
        );
    }
    let retention_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_jobs WHERE scope_key IN ($1,$2)")
            .bind(&owned_org_id)
            .bind(&orphan_org_id)
            .fetch_one(&pool)
            .await
            .expect("check that inventory made no jobs");
    assert_eq!(retention_jobs, 0);
    let resources_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organizations WHERE id=$1) \
         AND EXISTS(SELECT 1 FROM organizations WHERE id=$2) \
         AND EXISTS(SELECT 1 FROM projects WHERE id=$3) \
         AND EXISTS(SELECT 1 FROM environments WHERE id=$4) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$5) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$6) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$7) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$8) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$9) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$10) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$11)",
    )
    .bind(&owned_org_id)
    .bind(&orphan_org_id)
    .bind(&project_id)
    .bind(&environment_id)
    .bind(&active_free_id)
    .bind(&active_paid_id)
    .bind(&active_legacy_id)
    .bind(&consumed_id)
    .bind(&revoked_id)
    .bind(&expired_id)
    .bind(&multi_use_id)
    .fetch_one(&pool)
    .await
    .expect("check inventory did not delete resources");
    assert!(resources_remain);

    cleanup(&pool, &operator_id, &owned_org_id).await;
    cleanup(&pool, &member_id, &orphan_org_id).await;
}

#[tokio::test]
async fn expiry_dry_run_snapshots_only_personal_data_and_never_deletes_rows() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("retention-lifecycle-user-{suffix}");
    let other_user_id = format!("retention-lifecycle-other-{suffix}");
    let organisation_id = format!("retention-lifecycle-org-{suffix}");
    let project_id = format!("retention-lifecycle-project-{suffix}");
    let shared_personal_project_id =
        format!("retention-lifecycle-shared-personal-project-{suffix}");
    let shared_project_id = format!("retention-lifecycle-shared-project-{suffix}");
    let environment_id = format!("retention-lifecycle-env-{suffix}");
    let shared_personal_environment_id =
        format!("retention-lifecycle-shared-personal-env-{suffix}");
    let shared_environment_id = format!("retention-lifecycle-shared-env-{suffix}");
    let safe_share_id = format!("retention-lifecycle-share-safe-{suffix}");
    let late_free_share_id = format!("retention-lifecycle-share-free-late-{suffix}");
    let paid_late_share_id = format!("retention-lifecycle-share-paid-late-{suffix}");
    let paid_no_expiry_share_id = format!("retention-lifecycle-share-paid-no-expiry-{suffix}");
    let legacy_share_id = format!("retention-lifecycle-share-legacy-{suffix}");
    let consumed_share_id = format!("retention-lifecycle-share-consumed-{suffix}");
    let revoked_share_id = format!("retention-lifecycle-share-revoked-{suffix}");
    let expired_share_id = format!("retention-lifecycle-share-expired-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    cleanup(&pool, &other_user_id, &format!("unused-{suffix}")).await;
    seed_user(&pool, &user_id).await;
    seed_user(&pool, &other_user_id).await;
    let (export_start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('01','hex'))")
        .bind(&project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed personal project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         VALUES($1,$2,decode('01','hex'),3)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed personal environment");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) \
         VALUES($1,$2,decode('03','hex'))",
    )
    .bind(&environment_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed owner's environment grant");
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('06','hex'))")
        .bind(&shared_personal_project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed shared personal project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         VALUES($1,$2,decode('07','hex'),2)",
    )
    .bind(&shared_personal_environment_id)
    .bind(&shared_personal_project_id)
    .execute(&pool)
    .await
    .expect("seed shared personal environment");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key,granted_by) \
         VALUES($1,$2,decode('08','hex'),$3)",
    )
    .bind(&shared_personal_environment_id)
    .bind(&other_user_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("share personal environment with another user");
    sqlx::query("INSERT INTO organizations(id,enc_name) VALUES($1,decode('04','hex'))")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("seed organisation");
    sqlx::query(
        "INSERT INTO projects(id,owner_id,org_id,enc_name) \
         VALUES($1,$2,$3,decode('05','hex'))",
    )
    .bind(&shared_project_id)
    .bind(&user_id)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("seed organisation-owned project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         VALUES($1,$2,decode('01','hex'),1)",
    )
    .bind(&shared_environment_id)
    .bind(&shared_project_id)
    .execute(&pool)
    .await
    .expect("seed organisation-owned environment");
    for (id, class, view_count, expires_at, revoked_at) in [
        (&safe_share_id, "free", 0, Some(deadline - 1), None),
        (
            &late_free_share_id,
            "free",
            0,
            Some(deadline + 86_400),
            None,
        ),
        (
            &paid_late_share_id,
            "paid",
            0,
            Some(deadline + 86_400),
            None,
        ),
        (&paid_no_expiry_share_id, "paid", 0, None, None),
        (&legacy_share_id, "legacy", 0, None, None),
        (&consumed_share_id, "free", 1, Some(deadline + 86_400), None),
        (
            &revoked_share_id,
            "paid",
            0,
            Some(deadline + 86_400),
            Some(now),
        ),
        (&expired_share_id, "free", 0, Some(now - 100), None),
    ] {
        sqlx::query(
            "INSERT INTO share_links(id,token,enc_blob,created_by,max_views,view_count,expires_at,revoked_at,share_class) \
             VALUES($1,$2,decode('01','hex'),$3,1,$4,CASE WHEN $5::bigint IS NULL THEN NULL ELSE to_timestamp($5) END, \
                    CASE WHEN $6::bigint IS NULL THEN NULL ELSE to_timestamp($6) END,$7)",
        )
        .bind(id)
        .bind(format!("token-{id}"))
        .bind(&user_id)
        .bind(view_count)
        .bind(expires_at)
        .bind(revoked_at)
        .bind(class)
        .execute(&pool)
        .await
        .expect("seed share lifetime classification");
    }

    let state = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("reconcile expiry candidates");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load held expiry job before notice evidence");
    let initial_state: String =
        sqlx::query_scalar("SELECT state FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read initial job state");
    assert_eq!(initial_state, "held");

    // The notice producer can race the candidate scanner. A later pass must attach verified
    // notice evidence and release only the corresponding missing-notice hold. Expiry itself
    // cancels the current notice row after the window; its timestamp proves it was available at
    // the start, so the retention evidence must remain usable without leaving it in the inbox.
    seed_notice(&pool, &user_id, export_start, deadline, export_start - 60).await;
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=to_timestamp($2) \
         WHERE recipient_user_id=$1",
    )
    .bind(&user_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("expire the notice after its availability window");
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("attach later notice evidence");
    let job = sqlx::query(
        "SELECT job_id, state, purge_enabled, \
                extract(epoch from notice_evidence_at)::bigint AS notice_evidence_epoch, \
                operator_fingerprint \
         FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load expiry job");
    assert_eq!(job.try_get::<String, _>("job_id").expect("job id"), job_id);
    assert_eq!(job.get::<String, _>("state"), "ready");
    assert!(!job.get::<bool, _>("purge_enabled"));
    assert_eq!(
        job.get::<Option<i64>, _>("notice_evidence_epoch"),
        Some(export_start)
    );
    let fingerprint: String = job.try_get("operator_fingerprint").expect("fingerprint");
    assert_eq!(fingerprint.len(), 32);

    let scope = sqlx::query(
        "SELECT resource_kind,resource_id,state,hold_code,ownership_kind,expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_all(&pool)
    .await
    .expect("load scope");
    assert!(scope.iter().any(|row| {
        row.get::<String, _>("resource_kind") == "project"
            && row.get::<String, _>("resource_id") == project_id
            && row.get::<String, _>("state") == "planned"
    }));
    assert!(scope.iter().any(|row| {
        row.get::<String, _>("resource_kind") == "environment"
            && row.get::<String, _>("resource_id") == environment_id
            && row.get::<Option<i64>, _>("expected_revision") == Some(3)
    }));
    assert!(!scope.iter().any(|row| {
        row.get::<String, _>("resource_id") == shared_project_id
            || row.get::<String, _>("resource_id") == shared_environment_id
    }));
    assert!(scope.iter().any(|row| {
        row.get::<String, _>("resource_id") == shared_personal_project_id
            && row.get::<String, _>("state") == "held"
            && row.get::<Option<String>, _>("hold_code").as_deref() == Some("shared_resource")
    }));
    assert!(scope.iter().any(|row| {
        row.get::<String, _>("resource_id") == shared_personal_environment_id
            && row.get::<String, _>("state") == "held"
            && row.get::<Option<String>, _>("hold_code").as_deref() == Some("shared_resource")
    }));
    for share_id in [
        &paid_late_share_id,
        &paid_no_expiry_share_id,
        &legacy_share_id,
    ] {
        assert!(scope.iter().any(|row| {
            row.get::<String, _>("resource_id") == *share_id
                && row.get::<String, _>("state") == "planned"
        }));
    }
    assert!(scope.iter().any(|row| {
        row.get::<String, _>("resource_id") == late_free_share_id
            && row.get::<String, _>("state") == "held"
            && row.get::<String, _>("hold_code") == "free_share_promise_after_deadline"
    }));
    for eligible_share_id in [
        &safe_share_id,
        &paid_late_share_id,
        &paid_no_expiry_share_id,
        &legacy_share_id,
        &consumed_share_id,
        &revoked_share_id,
        &expired_share_id,
    ] {
        assert!(scope.iter().any(|row| {
            row.get::<String, _>("resource_id") == *eligible_share_id
                && row.get::<String, _>("state") == "planned"
        }));
    }

    let run = sotto_server::retention::run_dry_run_once(&pool, "retention-lifecycle-worker")
        .await
        .expect("run dry run")
        .expect("due job claimed");
    assert_eq!(run.state.as_str(), "dry_run");
    assert_eq!(run.held_items, 4);
    let dry_run_shares: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND resource_kind='share' AND state='dry_run'",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("count classified share links in dry run");
    assert_eq!(dry_run_shares, 7);
    let job = sqlx::query(
        "SELECT state,purge_enabled,(dry_run_completed_at IS NOT NULL) AS completed \
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("reload dry-run job");
    assert_eq!(job.get::<String, _>("state"), "dry_run");
    assert!(!job.get::<bool, _>("purge_enabled"));
    assert!(job.get::<bool, _>("completed"));
    let receipt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("count receipts");
    let tombstone_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_retention_tombstone_journal WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("count tombstones");
    assert_eq!(receipt_count, 0);
    assert_eq!(tombstone_count, 0);
    let resources_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) \
         AND EXISTS(SELECT 1 FROM environments WHERE id=$2) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$3) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$4) \
         AND EXISTS(SELECT 1 FROM projects WHERE id=$5) \
         AND EXISTS(SELECT 1 FROM environments WHERE id=$6) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$7) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$8) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$9) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$10) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$11) \
         AND EXISTS(SELECT 1 FROM share_links WHERE id=$12)",
    )
    .bind(&project_id)
    .bind(&environment_id)
    .bind(&safe_share_id)
    .bind(&late_free_share_id)
    .bind(&shared_personal_project_id)
    .bind(&shared_personal_environment_id)
    .bind(&paid_late_share_id)
    .bind(&paid_no_expiry_share_id)
    .bind(&legacy_share_id)
    .bind(&consumed_share_id)
    .bind(&revoked_share_id)
    .bind(&expired_share_id)
    .fetch_one(&pool)
    .await
    .expect("check original rows remain");
    assert!(resources_remain);

    let operator = sotto_server::retention_ops::router().with_state(state);
    let unauthorised = operator
        .clone()
        .oneshot(
            Request::get("/ops/cloud/retention")
                .body(Body::empty())
                .expect("build unauthorised operator request"),
        )
        .await
        .expect("unauthorised operator response");
    assert_eq!(unauthorised.status(), StatusCode::UNAUTHORIZED);
    let response = operator
        .clone()
        .oneshot(
            Request::get("/ops/cloud/retention?limit=10")
                .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
                .body(Body::empty())
                .expect("build operator list request"),
        )
        .await
        .expect("operator list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read operator list");
    let body = String::from_utf8(body.to_vec()).expect("operator JSON is UTF-8");
    assert!(body.contains(&fingerprint));
    assert!(!body.contains(&user_id));
    assert!(!body.contains(&project_id));
    assert!(!body.contains(&safe_share_id));

    for action in ["hold", "retry_dry_run", "cancel"] {
        let response = operator
            .clone()
            .oneshot(
                Request::post(format!("/ops/cloud/retention/{fingerprint}"))
                    .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        "{{\"operator\":\"retention reviewer\",\"action\":\"{action}\"}}"
                    )))
                    .expect("build operator action request"),
            )
            .await
            .expect("operator action response");
        assert_eq!(response.status(), StatusCode::OK, "action {action}");
    }
    let operator_actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_operator_actions WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("count operator actions");
    let purge_enabled: bool =
        sqlx::query_scalar("SELECT purge_enabled FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read purge flag after operator controls");
    assert_eq!(operator_actions, 3);
    assert!(!purge_enabled);
    cleanup(&pool, &user_id, &organisation_id).await;
    cleanup(&pool, &other_user_id, &format!("unused-{suffix}")).await;
}

#[tokio::test]
async fn personal_retention_purge_requires_signed_review_and_preserves_the_user() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("retention-purge-user-{suffix}");
    let project_id = format!("retention-purge-project-{suffix}");
    let environment_prefix = format!("retention-purge-environment-{suffix}-");
    let token_prefix = format!("retention-purge-token-{suffix}-");
    let organisation_id = format!("unused-retention-purge-org-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('11','hex'))")
        .bind(&project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed personal purge project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         SELECT $1 || n::text,$2,decode('12','hex'),1 \
         FROM generate_series(1,65) AS n",
    )
    .bind(&environment_prefix)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed 65 personal purge environments");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) \
         SELECT $1 || n::text,$2,decode('13','hex') \
         FROM generate_series(1,65) AS n",
    )
    .bind(&environment_prefix)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed owner keys for all purge environments");
    sqlx::query(
        "INSERT INTO machine_tokens(id,env_id,name,token_hash,public_key,enc_vault_key,created_by,expires_at,beneficiary_id,beneficiary_status) \
         SELECT $1 || n::text,$2 || n::text,'retention-test',\
                decode(md5($1 || n::text),'hex'),decode('14','hex'),decode('15','hex'),\
                $3,now()+interval '7 days',$3,'verified' \
         FROM generate_series(1,65) AS n",
    )
    .bind(&token_prefix)
    .bind(&environment_prefix)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed machine tokens for all purge environments");
    let (notice_start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, notice_start, deadline, notice_start - 60).await;
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now() \
         WHERE recipient_user_id=$1 AND event_key=$2",
    )
    .bind(&user_id)
    .bind(format!("cloud-export-window:{notice_start}:{deadline}"))
    .execute(&pool)
    .await
    .expect("retain notice evidence after its delivery window");
    let app = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&app, &user_id)
        .await
        .expect("prepare personal retention scope");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load personal retention job");
    loop {
        let run = retention::run_dry_run_once(&pool, "personal-retention-dry-run")
            .await
            .expect("run personal dry-run batch")
            .expect("claim personal dry-run batch");
        if run.state.as_str() == "dry_run" {
            break;
        }
        assert_eq!(run.state.as_str(), "ready");
    }
    let scope_fingerprint = retention::review_scope_fingerprint(&pool, &job_id)
        .await
        .expect("fingerprint exact personal scope");
    let release_sha = "d".repeat(40);
    let evidence_key = b"separate personal retention restore evidence key";
    let journal_entry_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&pool)
            .await
            .expect("count current tombstones");
    let mut evidence = retention::RestoreEvidenceAttestation {
        evidence_id: format!("restore-evidence:personal-{suffix}"),
        release_sha: release_sha.clone(),
        backup_fingerprint: "e".repeat(64),
        sidecar_sha256: "f".repeat(64),
        journal_entry_count,
        replay_entry_count: journal_entry_count,
        first_replay_deleted_count: journal_entry_count,
        second_replay_deleted_count: 0,
        incarnation_guard_verified: true,
        signature: String::new(),
    };
    evidence.signature = retention::sign_restore_evidence(&evidence, evidence_key);
    retention::register_restore_evidence(
        &pool,
        &evidence,
        evidence_key,
        &release_sha,
        "retention-test",
    )
    .await
    .expect("register signed restore evidence");
    retention::approve_purge(
        &pool,
        &job_id,
        &scope_fingerprint,
        &evidence.evidence_id,
        "retention-reviewer",
        &release_sha,
    )
    .await
    .expect("approve exact reviewed personal scope");
    let purge =
        retention::run_approved_purge_once(&pool, "personal-retention-purge-worker", &release_sha)
            .await
            .expect("execute approved personal purge")
            .expect("claim approved personal purge");
    assert_eq!(purge.state.as_str(), "held");
    assert!(purge.processed_items > 64);
    let remaining_environment_keys: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE \
             NOT EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=e.id AND g.user_id=$2) \
             OR NOT EXISTS (SELECT 1 FROM machine_tokens m WHERE m.env_id=e.id)) \
         FROM environments e WHERE e.id LIKE $1 || '%'",
    )
    .bind(&environment_prefix)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("check every surviving environment still has its key material");
    assert_eq!(remaining_environment_keys, (1, 0));
    let first_batch: (i64, i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1), \
                (SELECT count(*) FROM cloud_retention_tombstone_journal WHERE job_id=$1), \
                (SELECT state FROM cloud_retention_purge_approvals WHERE job_id=$1 \
                 ORDER BY approved_at DESC LIMIT 1)",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("inspect first bounded purge batch");
    assert_eq!(first_batch.0, purge.processed_items as i64);
    assert_eq!(first_batch.1, purge.processed_items as i64);
    assert_eq!(first_batch.2, "stale");
    assert!(matches!(
        retention::run_approved_purge_once(&pool, "personal-retention-purge-worker", &release_sha)
            .await,
        Ok(None)
    ));
    let updated_journal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&pool)
            .await
            .expect("count tombstones after first purge batch");
    let mut updated_evidence = retention::RestoreEvidenceAttestation {
        evidence_id: format!("restore-evidence:personal-follow-up-{suffix}"),
        release_sha: release_sha.clone(),
        backup_fingerprint: "1".repeat(64),
        sidecar_sha256: "2".repeat(64),
        journal_entry_count: updated_journal_count,
        replay_entry_count: updated_journal_count,
        first_replay_deleted_count: updated_journal_count,
        second_replay_deleted_count: 0,
        incarnation_guard_verified: true,
        signature: String::new(),
    };
    updated_evidence.signature = retention::sign_restore_evidence(&updated_evidence, evidence_key);
    retention::register_restore_evidence(
        &pool,
        &updated_evidence,
        evidence_key,
        &release_sha,
        "retention-test",
    )
    .await
    .expect("register fresh restore evidence after journal changed");
    retention::approve_purge(
        &pool,
        &job_id,
        &scope_fingerprint,
        &updated_evidence.evidence_id,
        "retention-reviewer",
        &release_sha,
    )
    .await
    .expect("require fresh approval after first bounded batch");
    let final_purge =
        retention::run_approved_purge_once(&pool, "personal-retention-purge-worker", &release_sha)
            .await
            .expect("execute second approved personal purge batch")
            .expect("claim second approved personal purge batch");
    assert_eq!(final_purge.state.as_str(), "completed");
    let users_remain: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("verify account remains intact");
    assert!(users_remain);
    let resources_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) OR \
                EXISTS(SELECT 1 FROM environments WHERE id LIKE $2 || '%') OR \
                EXISTS(SELECT 1 FROM environment_grants WHERE env_id LIKE $2 || '%') OR \
                EXISTS(SELECT 1 FROM machine_tokens WHERE id LIKE $3 || '%')",
    )
    .bind(&project_id)
    .bind(&environment_prefix)
    .bind(&token_prefix)
    .fetch_one(&pool)
    .await
    .expect("verify personal scope children and parents were deleted");
    assert!(!resources_remain);
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn new_paid_coverage_cancels_a_prepared_expiry_episode() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("retention-resumed-user-{}", uuid::Uuid::new_v4());
    let organisation_id = format!("retention-resumed-org-{}", uuid::Uuid::new_v4());
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start - 60).await;
    let state = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("enqueue expired episode");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load queued job");
    publish_authoritative_coverage(
        &pool,
        &user_id,
        vec![ConfirmedPaidInterval {
            coverage_id: format!("coverage:{user_id}:renewed"),
            source_id: format!("source:{user_id}"),
            starts_at: now - 60,
            paid_until: now + 30 * 24 * 60 * 60,
            failed_renewal_id: None,
        }],
        now,
        "renewed",
    )
    .await;
    let run = sotto_server::retention::run_dry_run_once(&pool, "retention-resumed-worker")
        .await
        .expect("recheck candidate against new coverage")
        .expect("candidate claimed");
    assert_eq!(run.state.as_str(), "held");
    let hold: Option<String> =
        sqlx::query_scalar("SELECT hold_code FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read coverage-change hold");
    assert_eq!(hold.as_deref(), Some("coverage_revision_changed"));
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("cancel superseded expiry");
    let state: String =
        sqlx::query_scalar("SELECT state FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read cancelled job");
    assert_eq!(state, "cancelled");
    let notice_event_key = format!(
        "cloud-export-window:{}:{}",
        now + 30 * 24 * 60 * 60,
        now + 60 * 24 * 60 * 60
    );
    let notice_was_prepared: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
         AND event_key=$2 AND kind='export_deadline' AND channel='in_app' \
         AND due_at > created_at)",
    )
    .bind(&user_id)
    .bind(notice_event_key)
    .fetch_one(&pool)
    .await
    .expect("verify future export notice was made available ahead of the window");
    assert!(notice_was_prepared);
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn invalidated_authority_holds_a_prepared_expiry_episode() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("retention-authority-stale-{}", uuid::Uuid::new_v4());
    let organisation_id = format!("retention-authority-org-{}", uuid::Uuid::new_v4());
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start - 60).await;
    let state = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("enqueue expired episode");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load queued job");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators \
         SET provider_invalidation_generation = provider_invalidation_generation + 1 \
         WHERE beneficiary_id=$1",
    )
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("invalidate the authority without changing the projection");

    let run = sotto_server::retention::run_dry_run_once(&pool, "retention-authority-worker")
        .await
        .expect("hold the candidate when its authority is stale")
        .expect("candidate claimed");
    assert_eq!(run.state.as_str(), "held");
    let (hold, completed): (Option<String>, bool) = sqlx::query_as(
        "SELECT hold_code, dry_run_completed_at IS NOT NULL \
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("read authority hold");
    assert_eq!(hold.as_deref(), Some("coverage_authority_unavailable"));
    assert!(!completed);
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn late_export_notice_keeps_the_expiry_candidate_held() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("retention-late-notice-user-{}", uuid::Uuid::new_v4());
    let organisation_id = format!("retention-late-notice-org-{}", uuid::Uuid::new_v4());
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start + 1).await;
    let state = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("reconcile expiry with late notice");

    let candidate = sqlx::query(
        "SELECT state, hold_code, notice_evidence_at IS NULL AS evidence_missing \
         FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load held candidate");
    assert_eq!(candidate.get::<String, _>("state"), "held");
    assert_eq!(
        candidate.get::<Option<String>, _>("hold_code").as_deref(),
        Some("notice_evidence_missing")
    );
    assert!(candidate.get::<bool, _>("evidence_missing"));
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn changed_environment_revision_is_held_without_deletion() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("retention-drift-user-{suffix}");
    let organisation_id = format!("retention-drift-org-{suffix}");
    let project_id = format!("retention-drift-project-{suffix}");
    let late_project_id = format!("retention-drift-late-project-{suffix}");
    let environment_id = format!("retention-drift-env-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start - 60).await;
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('01','hex'))")
        .bind(&project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         VALUES($1,$2,decode('02','hex'),3)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed environment");
    let state = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&state, &user_id)
        .await
        .expect("enqueue expiry candidate");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load candidate job");
    sqlx::query("UPDATE environments SET revision=revision+1 WHERE id=$1")
        .bind(&environment_id)
        .execute(&pool)
        .await
        .expect("simulate environment drift after snapshot");

    let run = sotto_server::retention::run_dry_run_once(&pool, "retention-drift-worker")
        .await
        .expect("run dry run")
        .expect("candidate claimed");
    assert_eq!(run.state.as_str(), "held");
    let (hold, completed): (Option<String>, bool) = sqlx::query_as(
        "SELECT hold_code, dry_run_completed_at IS NOT NULL \
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("read held candidate");
    assert_eq!(hold.as_deref(), Some("resource_identity_changed"));
    assert!(!completed);

    // A new personal project created after the immutable snapshot must be detected on a later
    // operator retry rather than silently left outside the candidate.
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('03','hex'))")
        .bind(&late_project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("add project after candidate snapshot");
    sotto_server::retention::apply_operator_action(
        &pool,
        &job_id,
        "retention reviewer",
        sotto_server::retention::OperatorAction::RetryDryRun,
    )
    .await
    .expect("retry held dry run");
    let retry = sotto_server::retention::run_dry_run_once(&pool, "retention-drift-worker")
        .await
        .expect("recheck scope inventory")
        .expect("candidate claimed for retry");
    assert_eq!(retry.state.as_str(), "held");
    let hold: Option<String> =
        sqlx::query_scalar("SELECT hold_code FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read scope-change hold");
    assert_eq!(hold.as_deref(), Some("scope_changed"));
    let remains: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) \
         AND EXISTS(SELECT 1 FROM environments WHERE id=$2) \
         AND EXISTS(SELECT 1 FROM projects WHERE id=$3)",
    )
    .bind(&project_id)
    .bind(&environment_id)
    .bind(&late_project_id)
    .fetch_one(&pool)
    .await
    .expect("confirm drifted resources remain");
    assert!(remains);
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("confirm no receipts");
    assert_eq!(receipts, 0);
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn purge_checks_environment_identity_before_deleting_its_grant_or_token() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("retention-purge-drift-user-{suffix}");
    let organisation_id = format!("unused-retention-purge-drift-org-{suffix}");
    let project_id = format!("retention-purge-drift-project-{suffix}");
    let environment_id = format!("retention-purge-drift-env-{suffix}");
    let token_id = format!("retention-purge-drift-token-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start - 60).await;
    sqlx::query("INSERT INTO projects(id,owner_id,enc_name) VALUES($1,$2,decode('21','hex'))")
        .bind(&project_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed personal project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) \
         VALUES($1,$2,decode('22','hex'),7)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed personal environment");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) \
         VALUES($1,$2,decode('23','hex'))",
    )
    .bind(&environment_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed wrapped owner key");
    sqlx::query(
        "INSERT INTO machine_tokens(id,env_id,name,token_hash,public_key,enc_vault_key,created_by,expires_at,beneficiary_id,beneficiary_status) \
         VALUES($1,$2,'retention-test',$3,decode('24','hex'),decode('25','hex'),$4,now()+interval '7 days',$4,'verified')",
    )
    .bind(&token_id)
    .bind(&environment_id)
    .bind(suffix.as_bytes().to_vec())
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed environment token");

    let app = state(pool.clone());
    sotto_server::cloud_retention_lifecycle::reconcile_user(&app, &user_id)
        .await
        .expect("prepare personal retention scope");
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE scope_key=$1 AND scope_kind='personal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load personal retention job");
    let dry_run = retention::run_dry_run_once(&pool, "personal-retention-drift-dry-run")
        .await
        .expect("run personal dry-run")
        .expect("claim dry-run");
    assert_eq!(dry_run.state.as_str(), "dry_run");
    let fingerprint = retention::review_scope_fingerprint(&pool, &job_id)
        .await
        .expect("fingerprint exact scope");
    let release_sha = "a".repeat(40);
    let evidence_key = b"separate personal retention restore evidence key";
    let journal_entry_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&pool)
            .await
            .expect("count current tombstones");
    let mut evidence = retention::RestoreEvidenceAttestation {
        evidence_id: format!("restore-evidence:purge-drift-{suffix}"),
        release_sha: release_sha.clone(),
        backup_fingerprint: "b".repeat(64),
        sidecar_sha256: "c".repeat(64),
        journal_entry_count,
        replay_entry_count: journal_entry_count,
        first_replay_deleted_count: journal_entry_count,
        second_replay_deleted_count: 0,
        incarnation_guard_verified: true,
        signature: String::new(),
    };
    evidence.signature = retention::sign_restore_evidence(&evidence, evidence_key);
    retention::register_restore_evidence(
        &pool,
        &evidence,
        evidence_key,
        &release_sha,
        "retention-test",
    )
    .await
    .expect("register signed restore evidence");
    retention::approve_purge(
        &pool,
        &job_id,
        &fingerprint,
        &evidence.evidence_id,
        "retention-reviewer",
        &release_sha,
    )
    .await
    .expect("approve exact reviewed scope");

    sqlx::query("UPDATE environments SET revision=revision+1 WHERE id=$1")
        .bind(&environment_id)
        .execute(&pool)
        .await
        .expect("simulate environment change after approval");
    let purge =
        retention::run_approved_purge_once(&pool, "personal-retention-drift-purge", &release_sha)
            .await
            .expect("hold on changed environment identity")
            .expect("claim approved purge");
    assert_eq!(purge.state.as_str(), "held");
    assert_eq!(purge.processed_items, 0);
    assert_eq!(purge.held_items, 1);

    let (environment_exists, grants, tokens, receipts, tombstones): (bool, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM environments WHERE id=$2), \
                    (SELECT count(*) FROM environment_grants WHERE env_id=$2), \
                    (SELECT count(*) FROM machine_tokens WHERE id=$3), \
                    (SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1), \
                    (SELECT count(*) FROM cloud_retention_tombstone_journal WHERE job_id=$1)",
        )
        .bind(&job_id)
        .bind(&environment_id)
        .bind(&token_id)
        .fetch_one(&pool)
        .await
        .expect("verify resources survived identity hold");
    assert!(environment_exists);
    assert_eq!((grants, tokens, receipts, tombstones), (1, 1, 0, 0));
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn bounded_scan_is_dark_until_enabled_and_persists_its_cursor() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("!retention-scan-user-{}", uuid::Uuid::new_v4());
    let organisation_id = format!("retention-scan-org-{}", uuid::Uuid::new_v4());
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &user_id, &organisation_id).await;
    seed_user(&pool, &user_id).await;
    let (start, deadline) = seed_expired_projection(&pool, &user_id, now).await;
    seed_notice(&pool, &user_id, start, deadline, start - 60).await;
    let mut state = state(pool.clone());
    state.cloud_retention_dry_run_enabled = false;
    assert_eq!(
        sotto_server::cloud_retention_lifecycle::reconcile_once(&state)
            .await
            .expect("disabled scan is a no-op"),
        0
    );
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_jobs WHERE scope_key=$1")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("confirm disabled scan created no job");
    assert_eq!(before, 0);

    state.cloud_retention_dry_run_enabled = true;
    let scanned = sotto_server::cloud_retention_lifecycle::reconcile_once(&state)
        .await
        .expect("run bounded expiry scan");
    assert!(scanned > 0);
    let job_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_retention_jobs WHERE scope_key=$1)")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("confirm scanner produced the expiry candidate");
    assert!(job_exists);
    let scan_state: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT cursor_user_id, lease_owner FROM cloud_retention_scan_state \
         WHERE scan_key='personal_expiry'",
    )
    .fetch_one(&pool)
    .await
    .expect("read released scan lease");
    assert!(scan_state.1.is_none());
    if scanned == 32 {
        assert!(scan_state.0.is_some());
    } else {
        assert!(scan_state.0.is_none());
    }
    cleanup(&pool, &user_id, &organisation_id).await;
}

#[tokio::test]
async fn bounded_scan_holds_one_conflicted_user_and_continues_to_later_users() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let failed_user = format!("!retention-scan-a-{suffix}");
    let later_user = format!("!retention-scan-z-{suffix}");
    let failed_org = format!("retention-scan-a-org-{suffix}");
    let later_org = format!("retention-scan-z-org-{suffix}");
    cleanup(&pool, &failed_user, &failed_org).await;
    cleanup(&pool, &later_user, &later_org).await;
    sqlx::query(
        "INSERT INTO cloud_retention_scan_state(scan_key,cursor_user_id,lease_owner,lease_expires_at) \
         VALUES('personal_expiry',NULL,NULL,NULL) ON CONFLICT(scan_key) DO UPDATE SET \
         cursor_user_id=NULL,lease_owner=NULL,lease_expires_at=NULL",
    )
    .execute(&pool)
    .await
    .expect("reset the bounded scan cursor");

    let now: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    seed_user(&pool, &failed_user).await;
    seed_user(&pool, &later_user).await;
    let (failed_start, failed_deadline) = seed_expired_projection(&pool, &failed_user, now).await;
    let (later_start, later_deadline) = seed_expired_projection(&pool, &later_user, now).await;
    seed_notice(
        &pool,
        &failed_user,
        failed_start,
        failed_deadline,
        failed_start - 60,
    )
    .await;
    seed_notice(
        &pool,
        &later_user,
        later_start,
        later_deadline,
        later_start - 60,
    )
    .await;
    let failed_revision: i64 = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1",
    )
    .bind(&failed_user)
    .fetch_one(&pool)
    .await
    .expect("read user's current revision");
    let stale_intent = RetentionIntent {
        job_id: format!("retention:stale-identity-{suffix}"),
        scope_kind: ScopeKind::Personal,
        scope_key: failed_user.clone(),
        subject_user_id: Some(failed_user.clone()),
        organisation_id: None,
        eligibility_episode: format!("export-expiry:{failed_start}:{failed_deadline}"),
        deadline_epoch: failed_deadline - 1,
        notice_event_key: format!("cloud-export-window:{failed_start}:{failed_deadline}"),
        notice_evidence_epoch: Some(failed_start),
        expected_coverage_revision: Some(failed_revision),
        expected_scope_generation: None,
        eligibility_loss_epoch: None,
    };
    assert_eq!(
        retention::enqueue(&pool, &stale_intent)
            .await
            .expect("seed one conflicting immutable job identity"),
        EnqueueOutcome::Enqueued
    );

    let state = state(pool.clone());
    let scanned = sotto_server::cloud_retention_lifecycle::reconcile_once(&state)
        .await
        .expect("one user conflict must not abort the bounded scan");
    assert!(scanned >= 2, "scan should reach both fixture users");
    let held: (String, Option<String>) =
        sqlx::query_as("SELECT state,hold_code FROM cloud_retention_jobs WHERE scope_key=$1")
            .bind(&failed_user)
            .fetch_one(&pool)
            .await
            .expect("read held conflicting candidate");
    assert_eq!(held.0, "held");
    assert_eq!(held.1.as_deref(), Some("reconciliation_failed"));
    let later_job_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_retention_jobs WHERE scope_key=$1)")
            .bind(&later_user)
            .fetch_one(&pool)
            .await
            .expect("read candidate after the failing user");
    assert!(later_job_exists, "the scan must continue to the next user");

    cleanup(&pool, &failed_user, &failed_org).await;
    cleanup(&pool, &later_user, &later_org).await;
    sqlx::query(
        "UPDATE cloud_retention_scan_state SET cursor_user_id=NULL,lease_owner=NULL,lease_expires_at=NULL \
         WHERE scan_key='personal_expiry'",
    )
    .execute(&pool)
    .await
    .expect("restore the scan cursor");
}

#[tokio::test]
async fn shared_retention_waits_for_every_admin_and_snapshots_the_full_shared_scope() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let owner_id = format!("shared-retention-owner-{suffix}");
    let admin_id = format!("shared-retention-admin-{suffix}");
    let eligible_id = format!("shared-retention-eligible-{suffix}");
    let organisation_id = format!("~shared-retention-org-{suffix}");
    let eligible_org_id = format!("~shared-retention-active-org-{suffix}");
    let project_id = format!("shared-retention-project-{suffix}");
    let environment_id = format!("shared-retention-environment-{suffix}");
    let token_id = format!("shared-retention-machine-token-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    for org_id in [&organisation_id, &eligible_org_id] {
        sqlx::query("DELETE FROM cloud_notice_outbox WHERE scope_id=$1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("remove previous shared notices");
        sqlx::query(
            "DELETE FROM cloud_retention_operator_actions WHERE job_id IN \
             (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
        )
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("remove previous shared operator actions");
        sqlx::query(
            "DELETE FROM cloud_retention_purge_approvals WHERE job_id IN \
             (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
        )
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("remove previous shared purge approvals");
        sqlx::query(
            "DELETE FROM cloud_retention_receipts WHERE job_id IN \
             (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
        )
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("remove previous shared purge receipts");
        sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("remove previous shared retention jobs");
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("remove previous test organisation");
    }
    for user_id in [&owner_id, &admin_id, &eligible_id] {
        cleanup(&pool, user_id, &organisation_id).await;
        seed_user(&pool, user_id).await;
    }
    // The scanner may first discover a real eligibility loss near the end of the 30-day
    // policy window. That must still produce a full notice window from the queued notice.
    let loss_epoch = now - EXPORT_WINDOW_SECONDS + 86_400;
    seed_coverage_until(&pool, &owner_id, loss_epoch, now, "shared-owner-expired").await;
    seed_coverage_until(&pool, &admin_id, loss_epoch, now, "shared-admin-expired").await;
    seed_sponsored_coverage_until(&pool, &eligible_id, &eligible_org_id, now + 86_400, now).await;

    sqlx::query("INSERT INTO organizations(id,enc_name) VALUES($1,decode('01','hex')),($2,decode('02','hex'))")
        .bind(&organisation_id)
        .bind(&eligible_org_id)
        .execute(&pool)
        .await
        .expect("seed shared organisations");
    sqlx::query(
        "INSERT INTO organization_memberships(org_id,user_id,role) \
         VALUES($1,$2,'owner'),($1,$3,'admin'),($4,$2,'owner'),($4,$5,'admin')",
    )
    .bind(&organisation_id)
    .bind(&owner_id)
    .bind(&admin_id)
    .bind(&eligible_org_id)
    .bind(&eligible_id)
    .execute(&pool)
    .await
    .expect("seed organisation roles, including mixed-eligibility administrators");
    sqlx::query(
        "INSERT INTO projects(id,owner_id,org_id,enc_name) VALUES($1,$2,$3,decode('03','hex'))",
    )
    .bind(&project_id)
    .bind(&owner_id)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("seed shared project");
    sqlx::query(
        "INSERT INTO environments(id,project_id,enc_name,revision) VALUES($1,$2,decode('04','hex'),7)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed shared environment");
    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) VALUES($1,$2,decode('05','hex'))",
    )
    .bind(&environment_id)
    .bind(&admin_id)
    .execute(&pool)
    .await
    .expect("seed shared grant");
    sqlx::query(
        "INSERT INTO machine_tokens \
         (id,env_id,name,token_hash,public_key,enc_vault_key,created_by,expires_at,beneficiary_id,beneficiary_status) \
         VALUES($1,$2,'retention-test',$4,decode('07','hex'),decode('08','hex'),$3,now()+interval '7 days',$3,'verified')",
    )
    .bind(&token_id)
    .bind(&environment_id)
    .bind(&owner_id)
    .bind(suffix.as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("seed shared machine token");

    let mut app = state(pool.clone());
    // The personal switch alone must not run the organisation-wide scan.
    app.cloud_shared_retention_dry_run_enabled = false;
    assert_eq!(
        sotto_server::cloud_shared_retention::reconcile_once(&app)
            .await
            .expect("shared scan stays disabled with only personal retention enabled"),
        0
    );
    // The shared switch works independently of the personal retention switch.
    app.cloud_retention_dry_run_enabled = false;
    app.cloud_shared_retention_dry_run_enabled = true;
    scan_shared_organisation(&app, &eligible_org_id).await;
    let premature: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&eligible_org_id)
    .fetch_one(&pool)
    .await
    .expect("check active admin prevents a shared candidate");
    assert_eq!(premature, 0);
    let sponsored_source: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cloud_provider_allocations allocation \
         JOIN cloud_provider_payers payer USING (payer_id) \
         WHERE allocation.beneficiary_id=$1 AND payer.payer_kind='sponsor' \
           AND payer.payer_id=$2)",
    )
    .bind(&eligible_id)
    .bind(&eligible_org_id)
    .fetch_one(&pool)
    .await
    .expect("confirm active administrator's coverage is sponsored");
    assert!(sponsored_source);

    scan_shared_organisation(&app, &organisation_id).await;
    let held_reason: Option<String> = sqlx::query_scalar(
        "SELECT hold_code FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&organisation_id)
    .fetch_optional(&pool)
    .await
    .expect("read no-contact hold")
    .flatten();
    assert_eq!(held_reason.as_deref(), Some("no_verified_owner_contact"));
    let no_contact_evidence: Option<i64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM notice_evidence_at)::bigint FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&organisation_id)
    .fetch_optional(&pool)
    .await
    .expect("read notice evidence")
    .flatten();
    assert_eq!(no_contact_evidence, None);
    let premature_notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE scope_id=$1 AND kind='shared_export_deadline'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("check that an uncontactable organisation has no deadline notices");
    assert_eq!(premature_notices, 0);

    sqlx::query(
        "INSERT INTO cloud_verified_contacts(contact_id,user_id,channel,destination,verified_at) \
         VALUES($1,$2,'email',$3,now())",
    )
    .bind(format!("shared-retention-contact-{suffix}"))
    .bind(&owner_id)
    .bind(format!("owner-{suffix}@example.test"))
    .execute(&pool)
    .await
    .expect("seed one reachable current administrator");
    scan_shared_organisation(&app, &organisation_id).await;
    let (job_id, state_name, deadline, evidence): (String, String, i64, Option<i64>) =
        sqlx::query_as(
            "SELECT job_id,state,extract(epoch FROM deadline_at)::bigint, \
                    extract(epoch FROM notice_evidence_at)::bigint \
             FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation' \
               AND state<>'cancelled'",
        )
        .bind(&organisation_id)
        .fetch_one(&pool)
        .await
        .expect("read qualified shared candidate");
    assert_eq!(state_name, "ready");
    let notice_created_epoch: i64 = sqlx::query_scalar(
        "SELECT ceil(extract(epoch FROM min(created_at)))::bigint FROM cloud_notice_outbox \
         WHERE scope_id=$1 AND kind='shared_export_deadline' AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("read first queued shared notice time");
    assert_eq!(deadline, notice_created_epoch + EXPORT_WINDOW_SECONDS);
    assert_eq!(evidence, Some(notice_created_epoch));
    let raw_loss: i64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM eligibility_loss_at)::bigint FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("read original eligibility loss boundary");
    assert_eq!(raw_loss, loss_epoch);

    let notice_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT recipient_user_id) FROM cloud_notice_outbox \
         WHERE scope_id=$1 AND kind='shared_export_deadline' AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("count notices to all current administrators");
    assert_eq!(notice_count, 2);
    let notice_rows_before_refresh: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE scope_id=$1 \
         AND kind='shared_export_deadline' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("count active notices before refreshing the shared scope");
    let original_event_key: String =
        sqlx::query_scalar("SELECT notice_event_key FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read stable shared notice identity");
    let scope: Vec<(String, String)> = sqlx::query_as(
        "SELECT resource_kind,resource_id FROM cloud_retention_scope_items WHERE job_id=$1 ORDER BY resource_kind",
    )
    .bind(&job_id)
    .fetch_all(&pool)
    .await
    .expect("read complete shared scope");
    assert_eq!(scope.len(), 4);
    assert!(scope.iter().any(|(kind, _)| kind == "project"));
    assert!(scope.iter().any(|(kind, _)| kind == "environment"));
    assert!(scope.iter().any(|(kind, _)| kind == "grant"));
    assert!(scope.iter().any(|(kind, _)| kind == "machine_token"));
    let resources_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) AND \
                EXISTS(SELECT 1 FROM environments WHERE id=$2) AND \
                EXISTS(SELECT 1 FROM environment_grants WHERE env_id=$2) AND \
                EXISTS(SELECT 1 FROM machine_tokens WHERE id=$3)",
    )
    .bind(&project_id)
    .bind(&environment_id)
    .bind(&token_id)
    .fetch_one(&pool)
    .await
    .expect("confirm dry-run snapshot did not delete resources");
    assert!(resources_remain);

    sqlx::query(
        "INSERT INTO environment_grants(env_id,user_id,enc_vault_key) VALUES($1,$2,decode('09','hex'))",
    )
    .bind(&environment_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .expect("add a grant after the first scope snapshot");
    scan_shared_organisation(&app, &organisation_id).await;
    let (
        refreshed_job_id,
        current_generation,
        refreshed_generation,
        refreshed_items,
        refreshed_deadline,
        refreshed_evidence,
        refreshed_event_key,
    ): (String, i64, i64, i64, i64, Option<i64>, String) = sqlx::query_as(
            "SELECT job.job_id,org.retention_generation,job.expected_scope_generation, \
                    (SELECT count(*) FROM cloud_retention_scope_items item WHERE item.job_id=job.job_id), \
                    extract(epoch FROM job.deadline_at)::bigint, \
                    extract(epoch FROM job.notice_evidence_at)::bigint,job.notice_event_key \
             FROM organizations org JOIN cloud_retention_jobs job ON job.organisation_id=org.id \
             WHERE org.id=$1 AND job.scope_kind='shared_organisation' AND job.state='ready'",
        )
        .bind(&organisation_id)
        .fetch_one(&pool)
        .await
        .expect("read refreshed shared scope");
    assert_eq!(current_generation, refreshed_generation);
    assert_eq!(refreshed_job_id, job_id);
    assert_eq!(refreshed_items, 5);
    assert_eq!(refreshed_deadline, deadline);
    assert_eq!(refreshed_evidence, evidence);
    assert_eq!(refreshed_event_key, original_event_key);
    let notice_rows_after_refresh: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE scope_id=$1 \
         AND kind='shared_export_deadline' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("count active notices after refreshing the shared scope");
    assert_eq!(notice_rows_after_refresh, notice_rows_before_refresh);

    // A genuinely new loss boundary starts a new episode and cancels the old pending
    // notices; a routine scope-generation bump above does neither.
    let next_loss_epoch = loss_epoch - 3_600;
    for user_id in [&owner_id, &admin_id] {
        publish_authoritative_coverage(
            &pool,
            user_id,
            vec![ConfirmedPaidInterval {
                coverage_id: format!("coverage:{user_id}"),
                source_id: format!("source:{user_id}"),
                starts_at: next_loss_epoch - 86_400,
                paid_until: next_loss_epoch,
                failed_renewal_id: None,
            }],
            now,
            "shared-retention-new-loss-boundary",
        )
        .await;
    }
    scan_shared_organisation(&app, &organisation_id).await;
    let (dry_run_job_id, new_event_key, new_deadline): (String, String, i64) = sqlx::query_as(
        "SELECT job_id,notice_event_key,extract(epoch FROM deadline_at)::bigint \
         FROM cloud_retention_jobs WHERE organisation_id=$1 \
           AND scope_kind='shared_organisation' AND state='ready'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("read the new shared loss episode");
    assert_ne!(dry_run_job_id, refreshed_job_id);
    assert_ne!(new_event_key, original_event_key);
    assert!(new_deadline >= deadline);
    let cancelled_old_notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE scope_id=$1 AND event_key=$2 \
         AND kind='shared_export_deadline' AND status='cancelled'",
    )
    .bind(&organisation_id)
    .bind(&original_event_key)
    .fetch_one(&pool)
    .await
    .expect("count notices cancelled with the superseded loss episode");
    assert_eq!(cancelled_old_notices, notice_rows_before_refresh);
    let active_new_notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE scope_id=$1 AND event_key=$2 \
         AND kind='shared_export_deadline' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .bind(&new_event_key)
    .fetch_one(&pool)
    .await
    .expect("count notices for the new loss episode");
    assert_eq!(active_new_notices, notice_rows_before_refresh);

    // Exercise the actual shared worker against a due, fully evidenced candidate. Move the
    // authoritative coverage boundary and the matching notice evidence together to model a
    // restart after the 30-day window; the worker must validate every scope row and stay dry-run.
    let dry_run_loss = now - 2 * EXPORT_WINDOW_SECONDS - 120;
    for user_id in [&owner_id, &admin_id] {
        publish_authoritative_coverage(
            &pool,
            user_id,
            vec![ConfirmedPaidInterval {
                coverage_id: format!("coverage:{user_id}"),
                source_id: format!("source:{user_id}"),
                starts_at: dry_run_loss - 86_400,
                paid_until: dry_run_loss,
                failed_renewal_id: None,
            }],
            now,
            "shared-dry-run-expired",
        )
        .await;
    }
    let dry_run_notice_epoch = now - EXPORT_WINDOW_SECONDS - 60;
    let dry_run_deadline = dry_run_notice_epoch + EXPORT_WINDOW_SECONDS;
    let event_material = format!(
        "{}:{}:{dry_run_loss}",
        organisation_id.len(),
        organisation_id
    );
    let dry_run_event_key = format!(
        "cloud-shared-export:{}",
        sha2::Sha256::digest(event_material.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query(
        "UPDATE cloud_notice_outbox SET event_key=$2,due_at=to_timestamp($3),created_at=to_timestamp($3), \
             payload=jsonb_set(payload,'{deadline_epoch}',to_jsonb($4::bigint)),updated_at=now() \
         WHERE scope_id=$1 AND event_key=(SELECT notice_event_key FROM cloud_retention_jobs WHERE job_id=$5) \
           AND kind='shared_export_deadline' AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .bind(&dry_run_event_key)
    .bind(dry_run_notice_epoch)
    .bind(dry_run_deadline)
    .bind(&dry_run_job_id)
    .execute(&pool)
    .await
    .expect("model the original notice evidence for the elapsed window");
    sqlx::query(
        "UPDATE cloud_retention_jobs SET eligibility_episode=$5,eligibility_loss_at=to_timestamp($6), \
             deadline_at=to_timestamp($2),notice_event_key=$3,notice_evidence_at=to_timestamp($4), \
             state='ready',hold_code=NULL \
         WHERE job_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&dry_run_job_id)
    .bind(dry_run_deadline)
    .bind(&dry_run_event_key)
    .bind(dry_run_notice_epoch)
    .bind(format!("shared-export:{dry_run_loss}"))
    .bind(dry_run_loss)
    .execute(&pool)
    .await
    .expect("make the fully evidenced candidate due for the shared dry run");
    sqlx::query(
        "UPDATE cloud_retention_jobs SET expected_scope_generation=( \
             SELECT retention_generation FROM organizations WHERE id=$2) WHERE job_id=$1",
    )
    .bind(&dry_run_job_id)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("refresh the candidate generation after reseeding coverage evidence");
    let run = retention::run_shared_dry_run_once(&pool, "shared-retention-lifecycle-test")
        .await
        .expect("run the bounded shared dry-run worker")
        .expect("claim the due shared candidate");
    assert_eq!(run.job_id, dry_run_job_id);
    let dry_run_hold: Option<String> =
        sqlx::query_scalar("SELECT hold_code FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&dry_run_job_id)
            .fetch_one(&pool)
            .await
            .expect("read shared dry-run hold, if any");
    assert_eq!(
        dry_run_hold, None,
        "shared dry run unexpectedly held: {dry_run_hold:?}"
    );
    assert_eq!(run.state.as_str(), "dry_run");
    assert_eq!(run.processed_items, 5);
    assert_eq!(run.held_items, 0);
    assert!(matches!(
        retention::run_once(
            &pool,
            "unapproved-purge-worker",
            retention::RetentionMode::Purge
        )
        .await,
        Err(retention::RetentionError::Held(_))
    ));
    let purge_enabled: bool =
        sqlx::query_scalar("SELECT purge_enabled FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&dry_run_job_id)
            .fetch_one(&pool)
            .await
            .expect("confirm shared dry run cannot enable purge");
    assert!(!purge_enabled);
    let dry_run_scope_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='dry_run'",
    )
    .bind(&dry_run_job_id)
    .fetch_one(&pool)
    .await
    .expect("read completed dry-run scope");
    assert_eq!(dry_run_scope_count, 5);
    let destructive_evidence_count: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1) + \
                (SELECT count(*) FROM cloud_retention_tombstone_journal WHERE job_id=$1)",
    )
    .bind(&dry_run_job_id)
    .fetch_one(&pool)
    .await
    .expect("check that dry run created no deletion evidence");
    assert_eq!(destructive_evidence_count, 0);
    let resources_remain_after_run: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) AND \
                EXISTS(SELECT 1 FROM environments WHERE id=$2) AND \
                EXISTS(SELECT 1 FROM environment_grants WHERE env_id=$2) AND \
                EXISTS(SELECT 1 FROM machine_tokens WHERE id=$3)",
    )
    .bind(&project_id)
    .bind(&environment_id)
    .bind(&token_id)
    .fetch_one(&pool)
    .await
    .expect("confirm shared dry run preserved every resource");
    assert!(resources_remain_after_run);

    retention::apply_operator_action(&pool, &dry_run_job_id, "reviewer", OperatorAction::Hold)
        .await
        .expect("hold the shared candidate for operator review");
    sqlx::query("UPDATE environments SET revision=revision+1 WHERE id=$1")
        .bind(&environment_id)
        .execute(&pool)
        .await
        .expect("bump scope generation after operator hold");
    scan_shared_organisation(&app, &organisation_id).await;
    let (held_state, held_reason, held_generation, held_scope_count):
        (String, Option<String>, i64, i64) = sqlx::query_as(
            "SELECT job.state,job.hold_code,job.expected_scope_generation, \
                    (SELECT count(*) FROM cloud_retention_scope_items item WHERE item.job_id=job.job_id) \
             FROM cloud_retention_jobs job WHERE job.job_id=$1",
        )
        .bind(&dry_run_job_id)
        .fetch_one(&pool)
        .await
        .expect("read operator-held job after refreshing its scope");
    assert_eq!(held_state, "held");
    assert_eq!(held_reason.as_deref(), Some("operator_hold"));
    let current_generation: i64 =
        sqlx::query_scalar("SELECT retention_generation FROM organizations WHERE id=$1")
            .bind(&organisation_id)
            .fetch_one(&pool)
            .await
            .expect("read generation after refreshing operator-held scope");
    assert_eq!(held_generation, current_generation);
    assert_eq!(held_scope_count, 5);

    retention::apply_operator_action(&pool, &dry_run_job_id, "reviewer", OperatorAction::Cancel)
        .await
        .expect("cancel the shared candidate");
    sqlx::query("UPDATE environments SET revision=revision+1 WHERE id=$1")
        .bind(&environment_id)
        .execute(&pool)
        .await
        .expect("bump scope generation after operator cancellation");
    scan_shared_organisation(&app, &organisation_id).await;
    let (cancelled_state, cancelled_reason, live_job_count):
        (String, Option<String>, i64) = sqlx::query_as(
            "SELECT job.state,job.hold_code, \
                    (SELECT count(*) FROM cloud_retention_jobs live \
                     WHERE live.organisation_id=job.organisation_id AND live.scope_kind='shared_organisation' \
                       AND live.state IN ('planned','ready','dry_run','held','leased')) \
             FROM cloud_retention_jobs job WHERE job.job_id=$1",
        )
        .bind(&dry_run_job_id)
        .fetch_one(&pool)
        .await
        .expect("read cancelled job after scope generation changed");
    assert_eq!(cancelled_state, "cancelled");
    assert_eq!(cancelled_reason.as_deref(), Some("operator_cancelled"));
    assert_eq!(live_job_count, 0);

    // Exercise the separately approved destructive path only after the non-destructive
    // operator hold/cancel assertions above. The test uses a signed restore attestation and
    // the exact dry-run scope fingerprint; an unapproved worker call cannot claim the job.
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='dry_run',dry_run=TRUE,purge_enabled=FALSE, \
             dry_run_completed_at=COALESCE(dry_run_completed_at,now()), \
             hold_code=NULL,execution_scope_generation=NULL, \
             expected_scope_generation=(SELECT retention_generation FROM organizations WHERE id=$2), \
             deadline_at=notice_evidence_at+interval '30 days' \
         WHERE job_id=$1",
    )
    .bind(&dry_run_job_id)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("prepare the completed dry run for its guarded purge review");
    sqlx::query(
        "UPDATE cloud_retention_scope_items SET state='dry_run',hold_code=NULL, \
             expected_revision=CASE WHEN resource_kind='environment' THEN \
                 (SELECT revision FROM environments WHERE id=resource_id) ELSE expected_revision END \
         WHERE job_id=$1",
    )
    .bind(&dry_run_job_id)
    .execute(&pool)
    .await
    .expect("refresh reviewed resource revisions after the test generation changes");
    let release_sha = "a".repeat(40);
    let evidence_key = b"test restore evidence key with at least thirty two bytes";
    let journal_entry_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&pool)
            .await
            .expect("count the current global tombstone journal");
    let mut restore_evidence = retention::RestoreEvidenceAttestation {
        evidence_id: format!("restore-evidence:shared-{suffix}"),
        release_sha: release_sha.clone(),
        backup_fingerprint: "b".repeat(64),
        sidecar_sha256: "c".repeat(64),
        journal_entry_count,
        replay_entry_count: journal_entry_count,
        first_replay_deleted_count: journal_entry_count,
        second_replay_deleted_count: 0,
        incarnation_guard_verified: true,
        signature: String::new(),
    };
    restore_evidence.signature = retention::sign_restore_evidence(&restore_evidence, evidence_key);
    retention::register_restore_evidence(
        &pool,
        &restore_evidence,
        evidence_key,
        &release_sha,
        "retention-test",
    )
    .await
    .expect("record independently signed restore evidence");
    let reviewed_scope = retention::review_scope_fingerprint(&pool, &dry_run_job_id)
        .await
        .expect("fingerprint the complete scope for explicit review");
    retention::approve_purge(
        &pool,
        &dry_run_job_id,
        &reviewed_scope,
        &restore_evidence.evidence_id,
        "retention-reviewer",
        &release_sha,
    )
    .await
    .expect("approve only the reviewed scope with current restore evidence");
    retention::apply_operator_action(
        &pool,
        &dry_run_job_id,
        "retention-reviewer",
        OperatorAction::Hold,
    )
    .await
    .expect("operator hold revokes the active destructive approval");
    let revoked_approval: String = sqlx::query_scalar(
        "SELECT state FROM cloud_retention_purge_approvals WHERE job_id=$1 ORDER BY approved_at DESC LIMIT 1",
    )
    .bind(&dry_run_job_id)
    .fetch_one(&pool)
    .await
    .expect("read revoked purge approval");
    assert_eq!(revoked_approval, "revoked");
    retention::approve_purge(
        &pool,
        &dry_run_job_id,
        &reviewed_scope,
        &restore_evidence.evidence_id,
        "retention-reviewer",
        &release_sha,
    )
    .await
    .expect("resume a held purge only through a fresh explicit approval");
    let purge =
        retention::run_approved_purge_once(&pool, "approved-shared-retention-worker", &release_sha)
            .await
            .expect("run the separately approved bounded purge")
            .expect("claim the approved due job");
    assert_eq!(purge.state.as_str(), "completed");
    let deletion_counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1), \
                (SELECT count(*) FROM cloud_retention_tombstone_journal WHERE job_id=$1)",
    )
    .bind(&dry_run_job_id)
    .fetch_one(&pool)
    .await
    .expect("verify atomic receipts and restore tombstones");
    assert_eq!(deletion_counts, (held_scope_count, held_scope_count));
    let protected_users_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1) AND EXISTS(SELECT 1 FROM users WHERE id=$2)",
    )
    .bind(&owner_id)
    .bind(&admin_id)
    .fetch_one(&pool)
    .await
    .expect("verify purging scoped data did not delete account rows");
    assert!(protected_users_remain);

    app.cloud_shared_retention_dry_run_enabled = false;
    sqlx::query("DELETE FROM cloud_notice_outbox WHERE scope_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove shared notices");
    sqlx::query(
        "DELETE FROM cloud_retention_operator_actions WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
    )
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("remove shared operator actions");
    sqlx::query(
        "DELETE FROM cloud_retention_purge_approvals WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
    )
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("remove shared purge approvals");
    sqlx::query(
        "DELETE FROM cloud_retention_receipts WHERE job_id IN \
         (SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1)",
    )
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("remove shared purge receipts");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove shared jobs");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
        .bind(&eligible_org_id)
        .execute(&pool)
        .await
        .expect("remove active organisation jobs");
    sqlx::query("DELETE FROM organizations WHERE id=ANY($1)")
        .bind(vec![organisation_id.clone(), eligible_org_id.clone()])
        .execute(&pool)
        .await
        .expect("remove shared test organisations");
    for user_id in [&owner_id, &admin_id, &eligible_id] {
        cleanup(&pool, user_id, &organisation_id).await;
    }
}

#[tokio::test]
async fn shared_dry_run_holds_when_scope_changes_while_waiting_for_organisation_lock() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let organisation_id = format!("~shared-retention-race-org-{suffix}");
    let owner_id = format!("shared-retention-race-owner-{suffix}");
    let writer_id = format!("shared-retention-race-writer-{suffix}");
    let original_project_id = format!("shared-retention-race-before-{suffix}");
    let added_project_id = format!("shared-retention-race-after-{suffix}");
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    cleanup(&pool, &owner_id, &organisation_id).await;
    cleanup(&pool, &writer_id, &organisation_id).await;
    sqlx::query("DELETE FROM cloud_notice_outbox WHERE scope_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove previous shared notices");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove previous shared retention jobs");
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove previous test organisation");
    seed_user(&pool, &owner_id).await;
    seed_user(&pool, &writer_id).await;
    let loss_epoch = now - 60;
    seed_coverage_until(&pool, &owner_id, loss_epoch, now, "shared-race-expired").await;
    sqlx::query("INSERT INTO organizations(id,enc_name) VALUES($1,decode('01','hex'))")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("seed shared organisation");
    sqlx::query("INSERT INTO organization_memberships(org_id,user_id,role) VALUES($1,$2,'owner')")
        .bind(&organisation_id)
        .bind(&owner_id)
        .execute(&pool)
        .await
        .expect("seed shared owner");
    sqlx::query(
        "INSERT INTO projects(id,owner_id,org_id,enc_name) VALUES($1,$2,$3,decode('02','hex'))",
    )
    .bind(&original_project_id)
    .bind(&owner_id)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("seed initial shared project");
    sqlx::query(
        "INSERT INTO cloud_verified_contacts(contact_id,user_id,channel,destination,verified_at) \
         VALUES($1,$2,'email',$3,now())",
    )
    .bind(format!("shared-race-contact-{suffix}"))
    .bind(&owner_id)
    .bind(format!("owner-{suffix}@example.test"))
    .execute(&pool)
    .await
    .expect("seed verified owner contact");

    let app = state(pool.clone());
    scan_shared_organisation_until_job(&app, &organisation_id).await;
    let job_id: String = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation' AND state='ready'",
    )
    .bind(&organisation_id)
    .fetch_one(&pool)
    .await
    .expect("prepare due shared retention candidate");

    let dry_run_loss = now - EXPORT_WINDOW_SECONDS - 120;
    publish_authoritative_coverage(
        &pool,
        &owner_id,
        vec![ConfirmedPaidInterval {
            coverage_id: format!("coverage:{owner_id}"),
            source_id: format!("source:{owner_id}"),
            starts_at: dry_run_loss - 86_400,
            paid_until: dry_run_loss,
            failed_renewal_id: None,
        }],
        now,
        "shared-race-dry-run-expired",
    )
    .await;
    let dry_run_deadline = dry_run_loss + EXPORT_WINDOW_SECONDS;
    let event_material = format!(
        "{}:{}:{dry_run_loss}",
        organisation_id.len(),
        organisation_id
    );
    let dry_run_event_key = format!(
        "cloud-shared-export:{}",
        sha2::Sha256::digest(event_material.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query(
        "UPDATE cloud_notice_outbox SET event_key=$2,due_at=to_timestamp($3),updated_at=now() \
         WHERE scope_id=$1 AND kind='shared_export_deadline' AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(&organisation_id)
    .bind(&dry_run_event_key)
    .bind(dry_run_loss)
    .execute(&pool)
    .await
    .expect("model the elapsed window notice evidence");
    sqlx::query(
        "UPDATE cloud_retention_jobs SET deadline_at=to_timestamp($2),notice_event_key=$3, \
             notice_evidence_at=to_timestamp($4),state='ready',hold_code=NULL,expected_scope_generation=( \
                 SELECT retention_generation FROM organizations WHERE id=$5) \
         WHERE job_id=$1",
    )
    .bind(&job_id)
    .bind(dry_run_deadline)
    .bind(&dry_run_event_key)
    .bind(dry_run_loss)
    .bind(&organisation_id)
    .execute(&pool)
    .await
    .expect("make fully evidenced candidate due for the shared dry run");

    let mut writer = pool.begin().await.expect("begin concurrent scope writer");
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(&organisation_id)
        .fetch_one(&mut *writer)
        .await
        .expect("hold organisation row for concurrent scope update");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writer)
        .await
        .expect("read scope writer backend pid");
    let worker_pool = pool.clone();
    let worker = tokio::spawn(async move {
        retention::run_shared_dry_run_once(&worker_pool, "shared-retention-race-worker").await
    });
    let _waiter_pid = wait_for_shared_worker_blocked(&pool, holder_pid).await;
    sqlx::query(
        "INSERT INTO projects(id,owner_id,org_id,enc_name) VALUES($1,$2,$3,decode('03','hex'))",
    )
    .bind(&added_project_id)
    .bind(&writer_id)
    .bind(&organisation_id)
    .execute(&mut *writer)
    .await
    .expect("change shared scope while worker waits");
    writer
        .commit()
        .await
        .expect("commit concurrent scope change");

    let run = worker
        .await
        .expect("join shared dry-run worker")
        .expect("shared dry-run succeeds")
        .expect("due candidate claimed");
    assert_eq!(run.job_id, job_id);
    assert_eq!(run.state.as_str(), "held");
    let hold_code: Option<String> =
        sqlx::query_scalar("SELECT hold_code FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("read scope-change hold");
    assert_eq!(hold_code.as_deref(), Some("scope_generation_changed"));
    let resources_remain: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1) AND EXISTS(SELECT 1 FROM projects WHERE id=$2)",
    )
    .bind(&original_project_id)
    .bind(&added_project_id)
    .fetch_one(&pool)
    .await
    .expect("confirm race test preserved both projects");
    assert!(resources_remain);

    sqlx::query("DELETE FROM cloud_notice_outbox WHERE scope_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove shared notices");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove shared jobs");
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(&organisation_id)
        .execute(&pool)
        .await
        .expect("remove shared test organisation");
    cleanup(&pool, &owner_id, &organisation_id).await;
    cleanup(&pool, &writer_id, &organisation_id).await;
}

#[tokio::test]
async fn shared_retention_holds_orphans_transfers_and_unproven_admin_eligibility() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let orphan_org = format!("~shared-retention-orphan-{suffix}");
    let transfer_org = format!("~shared-retention-transfer-{suffix}");
    let unknown_org = format!("~shared-retention-unknown-{suffix}");
    let transfer_actor = format!("shared-retention-transfer-actor-{suffix}");
    let transfer_beneficiary = format!("shared-retention-transfer-beneficiary-{suffix}");
    let unknown_admin = format!("shared-retention-unknown-admin-{suffix}");
    let app = state(pool.clone());

    for org_id in [&orphan_org, &transfer_org, &unknown_org] {
        sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=$1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("remove previous shared retention jobs");
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("remove previous test organisation");
    }
    for user_id in [&transfer_actor, &transfer_beneficiary, &unknown_admin] {
        cleanup(&pool, user_id, &transfer_org).await;
        seed_user(&pool, user_id).await;
    }
    sqlx::query(
        "INSERT INTO organizations(id,enc_name) VALUES($1,decode('01','hex')), \
             ($2,decode('02','hex')),($3,decode('03','hex'))",
    )
    .bind(&orphan_org)
    .bind(&transfer_org)
    .bind(&unknown_org)
    .execute(&pool)
    .await
    .expect("seed orphan, transfer and unproven organisations");
    sqlx::query(
        "INSERT INTO organization_memberships(org_id,user_id,role) VALUES($1,$2,'owner'),($3,$4,'owner')",
    )
    .bind(&transfer_org)
    .bind(&transfer_actor)
    .bind(&unknown_org)
    .bind(&unknown_admin)
    .execute(&pool)
    .await
    .expect("seed current admins for the held cases");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id,actor_user_id,counterparty_user_id,beneficiary_id,source_kind, \
          destination_kind,destination_organization_id,offer,quote_version,quote_expires_at_epoch, \
          consent_expires_at_epoch,effective_from,idempotency_key,request_hash,provider_idempotency_key,state) \
         VALUES($1,$2,$3,$3,'personal','sponsor',$4,'standard_monthly',1,9999999999, \
                9999999999,9999999999,$5,$6,$7,'pending')",
    )
    .bind(format!("transfer:{suffix}"))
    .bind(&transfer_actor)
    .bind(&transfer_beneficiary)
    .bind(&transfer_org)
    .bind(format!("idem:{suffix}"))
    .bind(format!("hash:{suffix}"))
    .bind(format!("provider-key:{suffix}"))
    .execute(&pool)
    .await
    .expect("seed an in-flight payer transfer");

    scan_shared_organisation(&app, &orphan_org).await;
    scan_shared_organisation(&app, &transfer_org).await;
    scan_shared_organisation(&app, &unknown_org).await;

    let orphan: (String, String, Option<String>) = sqlx::query_as(
        "SELECT scope_kind,state,hold_code FROM cloud_retention_jobs WHERE organisation_id=$1",
    )
    .bind(&orphan_org)
    .fetch_one(&pool)
    .await
    .expect("read orphan hold");
    assert_eq!(orphan.0, "orphan_organisation");
    assert_eq!(orphan.1, "held");
    assert_eq!(orphan.2.as_deref(), Some("orphan_without_owner_admin"));

    let transfer_hold: Option<String> = sqlx::query_scalar(
        "SELECT hold_code FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&transfer_org)
    .fetch_one(&pool)
    .await
    .expect("read pending-transfer hold");
    assert_eq!(
        transfer_hold.as_deref(),
        Some("ownership_transition_pending")
    );

    let unknown_hold: Option<String> = sqlx::query_scalar(
        "SELECT hold_code FROM cloud_retention_jobs WHERE organisation_id=$1 AND scope_kind='shared_organisation'",
    )
    .bind(&unknown_org)
    .fetch_one(&pool)
    .await
    .expect("read unproven-admin hold");
    assert_eq!(
        unknown_hold.as_deref(),
        Some("owner_admin_eligibility_unproven")
    );

    sqlx::query("DELETE FROM cloud_retention_jobs WHERE organisation_id=ANY($1)")
        .bind(vec![
            orphan_org.clone(),
            transfer_org.clone(),
            unknown_org.clone(),
        ])
        .execute(&pool)
        .await
        .expect("remove held retention jobs");
    sqlx::query("DELETE FROM billing_transfer_intents WHERE beneficiary_id=$1")
        .bind(&transfer_beneficiary)
        .execute(&pool)
        .await
        .expect("remove seeded payer transfer");
    sqlx::query("DELETE FROM organizations WHERE id=ANY($1)")
        .bind(vec![orphan_org, transfer_org.clone(), unknown_org])
        .execute(&pool)
        .await
        .expect("remove held-case organisations");
    cleanup(&pool, &transfer_actor, &transfer_org).await;
    cleanup(&pool, &transfer_beneficiary, &transfer_org).await;
    cleanup(&pool, &unknown_admin, &transfer_org).await;
}
