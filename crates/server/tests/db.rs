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

fn assert_constraint<T>(result: Result<T, sqlx::Error>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("expected PostgreSQL constraint {expected} to reject the query"),
        Err(error) => error,
    };
    let sqlx::Error::Database(database_error) = error else {
        panic!("expected PostgreSQL constraint {expected}, got {error}");
    };
    assert_eq!(database_error.constraint(), Some(expected));
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
async fn billing_webhook_tables_keep_event_ids_and_watermarks() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }

    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    sqlx::query(
        "DELETE FROM stripe_subscription_watermarks WHERE subscription_id = 'sub-db-order'",
    )
    .execute(&pool)
    .await
    .expect("cleanup subscription watermark");
    sqlx::query(
        "DELETE FROM stripe_webhook_events WHERE event_id IN ('evt-db-order-a', 'evt-db-order-b')",
    )
    .execute(&pool)
    .await
    .expect("cleanup webhook events");

    let inserted = sqlx::query(
        "INSERT INTO stripe_webhook_events (event_id, event_type, stripe_created, subscription_id) \
         VALUES ('evt-db-order-a', 'customer.subscription.updated', 10, 'sub-db-order') \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("insert webhook event");
    assert_eq!(inserted.rows_affected(), 1);
    let duplicate = sqlx::query(
        "INSERT INTO stripe_webhook_events (event_id, event_type, stripe_created, subscription_id) \
         VALUES ('evt-db-order-a', 'customer.subscription.updated', 10, 'sub-db-order') \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("deduplicate webhook event");
    assert_eq!(duplicate.rows_affected(), 0);

    sqlx::query(
        "INSERT INTO stripe_webhook_events (event_id, event_type, stripe_created, subscription_id) \
         VALUES ('evt-db-order-b', 'customer.subscription.updated', 11, 'sub-db-order')",
    )
    .execute(&pool)
    .await
    .expect("insert newer webhook event");
    sqlx::query(
        "INSERT INTO stripe_subscription_watermarks (subscription_id, stripe_created, event_id) \
         VALUES ('sub-db-order', 11, 'evt-db-order-b')",
    )
    .execute(&pool)
    .await
    .expect("insert subscription watermark");
    let watermark: (i64, String) = sqlx::query_as(
        "SELECT stripe_created, event_id FROM stripe_subscription_watermarks \
         WHERE subscription_id = 'sub-db-order'",
    )
    .fetch_one(&pool)
    .await
    .expect("read subscription watermark");
    assert_eq!(watermark, (11, "evt-db-order-b".into()));
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

    // A deleted row cannot retain billing or trial data that a stale webhook could later use.
    let invalid_tombstone = sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL, tier = 'team', trial_ends_at = now(), \
         stripe_customer_id = 'cus-tombstone', stripe_subscription_id = 'sub-tombstone' \
         WHERE id = $1",
    )
    .bind(org_id)
    .execute(&pool)
    .await;
    assert_constraint(invalid_tombstone, "organizations_lifecycle_tombstone_check");

    // Active rows must retain their encrypted name until the final tombstone transition.
    let invalid_active_name = sqlx::query("UPDATE organizations SET enc_name = NULL WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await;
    assert_constraint(
        invalid_active_name,
        "organizations_lifecycle_enc_name_check",
    );

    // Unknown lifecycle states must be rejected rather than treated as active or deleted.
    let invalid_lifecycle =
        sqlx::query("UPDATE organizations SET lifecycle_state = 'archived' WHERE id = $1")
            .bind(org_id)
            .execute(&pool)
            .await;
    assert_constraint(invalid_lifecycle, "organizations_lifecycle_state_check");

    // Deleted rows require a deletion timestamp in the same atomic transition.
    let invalid_deleted_at = sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = NULL, \
         enc_name = NULL WHERE id = $1",
    )
    .bind(org_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_deleted_at,
        "organizations_lifecycle_deleted_at_check",
    );

    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL WHERE id = $1",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("mark organisation deleted");
    let (
        lifecycle_state,
        has_deleted_at,
        no_enc_name,
        no_creator,
        is_free,
        no_trial,
        no_billing_ids,
    ): (String, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT lifecycle_state, deleted_at IS NOT NULL, enc_name IS NULL, \
         created_by IS NULL, tier = 'free', trial_ends_at IS NULL, \
         stripe_customer_id IS NULL AND stripe_subscription_id IS NULL \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .expect("read deleted organisation");
    assert_eq!(lifecycle_state, "deleted");
    assert!(has_deleted_at);
    assert!(no_enc_name);
    assert!(no_creator);
    assert!(is_free);
    assert!(no_trial);
    assert!(no_billing_ids);

    // Tombstones cannot regain encrypted names after the purge transition.
    let invalid_deleted_name = sqlx::query("UPDATE organizations SET enc_name = $2 WHERE id = $1")
        .bind(org_id)
        .bind(enc_name.as_slice())
        .execute(&pool)
        .await;
    assert_constraint(
        invalid_deleted_name,
        "organizations_lifecycle_enc_name_check",
    );

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

    // Expired leases must be discoverable without scanning all deletion history.
    let lease_index_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
         WHERE schemaname = current_schema() AND indexname = 'organization_deletions_lease_idx')",
    )
    .fetch_one(&pool)
    .await
    .expect("check lease index");
    assert!(lease_index_exists);

    // A failed operation must record where recovery resumes.
    let invalid_failed_resume = sqlx::query(
        "UPDATE organization_deletions SET state = 'failed', resume_state = NULL \
         WHERE id = $1::uuid",
    )
    .bind(active_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_failed_resume,
        "organization_deletions_failed_resume_state_check",
    );

    sqlx::query(
        "UPDATE organization_deletions SET state = 'failed', resume_state = 'cancelling_billing' \
         WHERE id = $1::uuid",
    )
    .bind(active_operation_id)
    .execute(&pool)
    .await
    .expect("record failed operation resume state");

    // Workflow states are a closed set so workers fail closed on a new or misspelled value.
    let invalid_state = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after) \
         VALUES ($1::uuid, $2, 'future', 'test-owner', now(), now() + interval '30 days')",
    )
    .bind(invalid_state_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert_constraint(invalid_state, "organization_deletions_state_check");

    // A cancelled timestamp must not predate the deletion request.
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
    assert_constraint(
        invalid_timestamp,
        "organization_deletions_terminal_timestamp_check",
    );

    // Operator observations must include an actor, reason, and evidence reference.
    let invalid_observation = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, cancelled_at, \
          billing_observation_source, billing_observed_by, billing_observation_reason) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'operator', 'operator-1', 'manual check')",
    )
    .bind(invalid_observation_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_observation,
        "organization_deletions_observation_actor_check",
    );

    // A lease owner without an expiry cannot be reclaimed safely after a worker crash.
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
    assert_constraint(invalid_lease, "organization_deletions_lease_pair_check");

    // Provider billing results always carry the time of the observation.
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
    assert_constraint(
        invalid_billing,
        "organization_deletions_billing_result_pair_check",
    );

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
    assert_constraint(duplicate_active, "organization_deletions_active_org_idx");

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
          billing_observed_by, billing_observation_reason, billing_observation_evidence) \
         VALUES ($1::uuid, $2, 'cancelled', 'test-owner', now(), now() + interval '30 days', \
                 now(), 'operator', 'missing', now(), 'operator-1', 'verified in Workbench', \
                 'https://dashboard.stripe.com/test/workbench/evt_123')",
    )
    .bind(operator_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("record an operator observation");

    // A completed operation must record when the purge finished.
    let invalid_completed = sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after) \
         VALUES ($1::uuid, $2, 'completed', 'test-owner', now(), now() + interval '30 days')",
    )
    .bind(invalid_completed_operation_id)
    .bind(org_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_completed,
        "organization_deletions_completed_timestamp_check",
    );

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

    // The retry, retention, and lease indexes cover every class of due work.
    let due_index_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() \
         AND indexname IN ('organization_deletions_retry_idx', \
                           'organization_deletions_retention_idx', \
                           'organization_deletions_lease_idx')",
    )
    .fetch_one(&pool)
    .await
    .expect("check deletion indexes");
    assert_eq!(due_index_count, 3);

    // Derived deadlines cannot move before their source timestamps.
    let invalid_purge_after = sqlx::query(
        "UPDATE organization_deletions SET purge_after = requested_at - interval '1 second' \
         WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_purge_after,
        "organization_deletions_purge_after_check",
    );

    let invalid_backup_expiry = sqlx::query(
        "UPDATE organization_deletions SET managed_backup_expiry_by = requested_at \
         WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_backup_expiry,
        "organization_deletions_backup_expiry_check",
    );

    let invalid_billing_timestamp = sqlx::query(
        "UPDATE organization_deletions SET billing_checked_at = requested_at - interval '1 second', \
         last_billing_state = 'terminal' WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_billing_timestamp,
        "organization_deletions_billing_checked_at_check",
    );

    let invalid_lease_expiry = sqlx::query(
        "UPDATE organization_deletions SET lease_owner = 'worker-1', \
         lease_expires_at = requested_at - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_lease_expiry,
        "organization_deletions_lease_expires_at_check",
    );

    let invalid_next_attempt = sqlx::query(
        "UPDATE organization_deletions SET next_attempt_at = requested_at - interval '1 second' \
         WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_next_attempt,
        "organization_deletions_next_attempt_at_check",
    );

    // Retry counters and optimistic-concurrency versions cannot be negative.
    let invalid_attempt_count =
        sqlx::query("UPDATE organization_deletions SET attempt_count = -1 WHERE id = $1::uuid")
            .bind(completed_operation_id)
            .execute(&pool)
            .await;
    assert_constraint(
        invalid_attempt_count,
        "organization_deletions_attempt_count_check",
    );

    let invalid_state_version =
        sqlx::query("UPDATE organization_deletions SET state_version = -1 WHERE id = $1::uuid")
            .bind(completed_operation_id)
            .execute(&pool)
            .await;
    assert_constraint(
        invalid_state_version,
        "organization_deletions_state_version_check",
    );

    // Enum fields reject values that the worker does not know how to reconcile.
    let invalid_resume_state = sqlx::query(
        "UPDATE organization_deletions SET resume_state = 'future' WHERE id = $1::uuid",
    )
    .bind(active_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_resume_state,
        "organization_deletions_resume_state_check",
    );

    let invalid_billing_state = sqlx::query(
        "UPDATE organization_deletions SET last_billing_state = 'future', \
         billing_checked_at = now() WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_billing_state,
        "organization_deletions_last_billing_state_check",
    );

    let invalid_observation_source = sqlx::query(
        "UPDATE organization_deletions SET billing_observation_source = 'future', \
         last_billing_state = 'terminal', billing_checked_at = now() WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_observation_source,
        "organization_deletions_billing_observation_source_check",
    );

    // A provider observation must include the result fields as one atomic record.
    let invalid_observation_result = sqlx::query(
        "UPDATE organization_deletions SET billing_observation_source = 'provider', \
         last_billing_state = NULL, billing_checked_at = NULL WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_observation_result,
        "organization_deletions_observation_result_check",
    );

    let invalid_completed_timestamp = sqlx::query(
        "UPDATE organization_deletions SET completed_at = requested_at - interval '1 second' \
         WHERE id = $1::uuid",
    )
    .bind(completed_operation_id)
    .execute(&pool)
    .await;
    assert_constraint(
        invalid_completed_timestamp,
        "organization_deletions_completed_timestamp_check",
    );

    // Deletion history keeps the organisation tombstone addressable, so the foreign key is RESTRICT.
    let invalid_organisation_delete = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await;
    assert_constraint(invalid_organisation_delete, "organization_deletions_org_fk");

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
