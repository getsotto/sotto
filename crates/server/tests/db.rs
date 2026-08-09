//! Database round-trip test.
//!
//! Runs only when `SOTTO_RUN_DB_TESTS=1` and `DATABASE_URL` points at a local Postgres instance
//! (the CI `server` job's Postgres service, or a local `docker compose up`); otherwise it skips,
//! so `cargo test --workspace` stays DB-free.

use sotto_server::db;
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

fn should_run_db_tests(database_url: &str) -> bool {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 not set");
        return false;
    }

    let options = PgConnectOptions::from_str(database_url).expect("parse DATABASE_URL");
    let host = options.get_host();
    assert!(
        matches!(host, "localhost" | "127.0.0.1" | "::1"),
        "refusing to run destructive DB tests against non-local host: {host}"
    );
    true
}

#[tokio::test]
async fn migrations_apply_and_user_round_trips() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }

    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    let id = "test-user-roundtrip";
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("cleanup");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind("github")
    .bind("12345")
    .bind("user@example.com")
    .execute(&pool)
    .await
    .expect("insert");

    let (provider, subject): (String, String) =
        sqlx::query_as("SELECT oauth_provider, oauth_subject FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("select");
    assert_eq!(provider, "github");
    assert_eq!(subject, "12345");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn organization_deletion_schema_enforces_tombstones_and_operations() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }

    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    let org_id = "test-organization-deletion-schema";
    let active_operation_id = "00000000-0000-0000-0000-000000000015";
    let cancelled_operation_id = "00000000-0000-0000-0000-000000000016";
    let invalid_state_operation_id = "00000000-0000-0000-0000-000000000017";
    let invalid_timestamp_operation_id = "00000000-0000-0000-0000-000000000018";
    let invalid_observation_operation_id = "00000000-0000-0000-0000-000000000019";
    let invalid_lease_operation_id = "00000000-0000-0000-0000-000000000020";
    let invalid_billing_operation_id = "00000000-0000-0000-0000-000000000021";
    let invalid_completed_operation_id = "00000000-0000-0000-0000-000000000022";
    let provider_operation_id = "00000000-0000-0000-0000-000000000023";
    let operator_operation_id = "00000000-0000-0000-0000-000000000024";
    let completed_operation_id = "00000000-0000-0000-0000-000000000025";
    let enc_name = b"encrypted-name";
    sqlx::query("DELETE FROM organization_deletions WHERE org_id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup deletion operations");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup organisation");

    let (lifecycle_state, no_deleted_at): (String, bool) = sqlx::query_as(
        "INSERT INTO organizations (id, enc_name) VALUES ($1, $2) \
         RETURNING lifecycle_state, deleted_at IS NULL",
    )
    .bind(org_id)
    .bind(enc_name.as_slice())
    .fetch_one(&pool)
    .await
    .expect("insert organisation fixture");
    assert_eq!(lifecycle_state, "active");
    assert!(no_deleted_at);

    let invalid_active_name = sqlx::query("UPDATE organizations SET enc_name = NULL WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await;
    assert!(invalid_active_name.is_err());

    let invalid_lifecycle =
        sqlx::query("UPDATE organizations SET lifecycle_state = 'deleted' WHERE id = $1")
            .bind(org_id)
            .execute(&pool)
            .await;
    assert!(invalid_lifecycle.is_err());

    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL WHERE id = $1",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("mark organisation deleted");
    let (lifecycle_state, has_deleted_at, no_enc_name): (String, bool, bool) = sqlx::query_as(
        "SELECT lifecycle_state, deleted_at IS NOT NULL, enc_name IS NULL \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .expect("read deleted organisation");
    assert_eq!(lifecycle_state, "deleted");
    assert!(has_deleted_at);
    assert!(no_enc_name);

    let invalid_deleted_name = sqlx::query("UPDATE organizations SET enc_name = $2 WHERE id = $1")
        .bind(org_id)
        .bind(enc_name.as_slice())
        .execute(&pool)
        .await;
    assert!(invalid_deleted_name.is_err());

    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'active', deleted_at = NULL, \
         enc_name = $2 WHERE id = $1",
    )
    .bind(org_id)
    .bind(enc_name.as_slice())
    .execute(&pool)
    .await
    .expect("restore organisation fixture");

    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after) \
         VALUES ($1::uuid, $2, 'requested', 'test-owner', now(), now() + interval '30 days')",
    )
    .bind(active_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("insert deletion operation");
    let (attempt_count, state_version): (i32, i64) = sqlx::query_as(
        "SELECT attempt_count, state_version FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(active_operation_id)
    .fetch_one(&pool)
    .await
    .expect("read deletion defaults");
    assert_eq!(attempt_count, 0);
    assert_eq!(state_version, 0);

    sqlx::query(
        "UPDATE organization_deletions SET state = 'failed', resume_state = 'cancelling_billing' \
         WHERE id = $1::uuid",
    )
    .bind(active_operation_id)
    .execute(&pool)
    .await
    .expect("record failed operation resume state");

    let invalid_state = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after) \
         VALUES ($1::uuid, $2, 'future', 'test-owner', now(), now() + interval '30 days')",
    )
    .bind(invalid_state_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_state.is_err());

    let invalid_timestamp = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now() - interval '1 day')",
    )
    .bind(invalid_timestamp_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_timestamp.is_err());

    let invalid_observation = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, \
          billing_observation_source) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'operator')",
    )
    .bind(invalid_observation_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_observation.is_err());

    let invalid_lease = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, lease_owner) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'worker-1')",
    )
    .bind(invalid_lease_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_lease.is_err());

    let invalid_billing = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, \
          billing_observation_source, last_billing_state) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'provider', 'terminal')",
    )
    .bind(invalid_billing_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_billing.is_err());

    let duplicate_active = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, resume_state) \
         VALUES ($1::uuid, $2, 'failed', 'test-owner', now(), now() + interval '30 days', \
                 'cancelling_billing')",
    )
    .bind(cancelled_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(duplicate_active.is_err());

    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', now())",
    )
    .bind(cancelled_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("allow a terminal replacement operation");

    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, \
          billing_observation_source, last_billing_state, billing_checked_at) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'provider', 'terminal', now())",
    )
    .bind(provider_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("record a provider observation");

    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, \
          billing_observation_source, last_billing_state, billing_checked_at, \
          billing_observed_by, billing_observation_reason) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'operator', 'missing', now(), 'operator-1', 'verified in Workbench')",
    )
    .bind(operator_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("record an operator observation");

    let invalid_completed = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after) \
         VALUES ($1::uuid, $2, 'completed', 'test-owner', now(), now() + interval '30 days')",
    )
    .bind(invalid_completed_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert!(invalid_completed.is_err());

    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, completed_at) \
         VALUES ($1::uuid, $2, 'completed', 'test-owner', now(), now() + interval '30 days', now())",
    )
    .bind(completed_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("record completed operation");

    sqlx::query("DELETE FROM organization_deletions WHERE org_id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup deletion operations");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup organisation");
}
