//! Upgrade coverage for consent expiry on payer-transfer invitations.

use sqlx::migrate::Migrator;
use sqlx::Row;

use sotto_server::db;

mod support {
    pub mod migrations;
}

use support::migrations::{migrator_before, DisposableDatabase};

fn migrator_before_consent_expiry() -> Migrator {
    migrator_before(62)
}

#[tokio::test]
async fn unbound_live_transfer_intents_are_parked_during_upgrade() {
    let Some(database) = DisposableDatabase::create().await else {
        return;
    };
    migrator_before(56)
        .run(&database.pool)
        .await
        .expect("apply schema before transfer execution migration");

    for user_id in ["transfer-upgrade-actor", "transfer-upgrade-counterparty"] {
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'migration', $1)",
        )
        .bind(user_id)
        .execute(&database.pool)
        .await
        .expect("insert transfer participant");
    }
    sqlx::query(
        "INSERT INTO organizations (id, enc_name) VALUES ('transfer-upgrade-org', '\\x00')",
    )
    .execute(&database.pool)
    .await
    .expect("insert transfer destination");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, effective_from, idempotency_key, request_hash, \
          provider_idempotency_key, state) \
         VALUES ('transfer:upgrade-live', 'transfer-upgrade-actor', \
                 'transfer-upgrade-counterparty', 'transfer-upgrade-actor', 'personal', \
                 'sponsor', 'transfer-upgrade-org', 'standard_monthly', 1, 2000000000, \
                 2000000100, 'upgrade-live', 'upgrade-live-hash', 'upgrade-live-provider', \
                 'awaiting_consent')",
    )
    .execute(&database.pool)
    .await
    .expect("insert pre-binding live intent");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, effective_from, idempotency_key, request_hash, \
          provider_idempotency_key, state, result_code) \
         VALUES ('transfer:upgrade-completed', 'transfer-upgrade-actor', \
                 'transfer-upgrade-counterparty', 'transfer-upgrade-actor', 'personal', \
                 'sponsor', 'transfer-upgrade-org', 'standard_monthly', 1, 2000000000, \
                 2000000100, 'upgrade-completed', 'upgrade-completed-hash', \
                 'upgrade-completed-provider', 'completed', 'completed')",
    )
    .execute(&database.pool)
    .await
    .expect("insert historical completed intent");

    db::migrate(&database.pool)
        .await
        .expect("apply transfer execution migration");
    let live: (String, Option<String>) = sqlx::query_as(
        "SELECT state, result_code FROM billing_transfer_intents \
         WHERE transfer_id = 'transfer:upgrade-live'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("read upgraded live intent");
    assert_eq!(
        live,
        ("failed".into(), Some("source_binding_unavailable".into()))
    );
    let completed: (String, Option<String>) = sqlx::query_as(
        "SELECT state, result_code FROM billing_transfer_intents \
         WHERE transfer_id = 'transfer:upgrade-completed'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("read historical completed intent");
    assert_eq!(completed, ("completed".into(), Some("completed".into())));
    database.cleanup().await;
}

#[tokio::test]
async fn consent_expiry_migration_preserves_existing_invitation_deadlines() {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping billing transfer migration test: SOTTO_RUN_DB_TESTS=1 not set");
        return;
    };
    migrator_before_consent_expiry()
        .run(&database.pool)
        .await
        .expect("migrate to the previous transfer schema");

    let actor = "transfer-consent-migration-actor";
    let counterparty = "transfer-consent-migration-counterparty";
    let beneficiary = "transfer-consent-migration-beneficiary";
    let org = "transfer-consent-migration-org";
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) \
         VALUES ($1, 'transfer-migration', $1), ($2, 'transfer-migration', $2), \
                ($3, 'transfer-migration', $3)",
    )
    .bind(actor)
    .bind(counterparty)
    .bind(beneficiary)
    .execute(&database.pool)
    .await
    .expect("insert transfer participants");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267','hex'), $2)")
        .bind(org)
        .bind(actor)
        .execute(&database.pool)
        .await
        .expect("insert destination organisation");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, effective_from, idempotency_key, request_hash, \
          provider_idempotency_key, source_provider_subscription_id, state) \
         VALUES ('transfer:consent-migration', $1, $2, $3, 'personal', 'sponsor', $4, \
                 'standard_monthly', 1, 1700000900, 1700001000, 'migration-key', \
                 'migration-hash', 'migration-provider-key', 'sub_migration', 'awaiting_consent')",
    )
    .bind(actor)
    .bind(counterparty)
    .bind(beneficiary)
    .bind(org)
    .execute(&database.pool)
    .await
    .expect("insert historical invitation with its original deadline");

    db::migrate(&database.pool)
        .await
        .expect("apply consent expiry migration");
    let row = sqlx::query(
        "SELECT quote_expires_at_epoch, consent_expires_at_epoch, state \
         FROM billing_transfer_intents WHERE transfer_id = 'transfer:consent-migration'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("read upgraded invitation");
    assert_eq!(row.get::<i64, _>("consent_expires_at_epoch"), 1_700_000_900);
    assert_eq!(row.get::<i64, _>("quote_expires_at_epoch"), 1_700_000_900);
    assert_eq!(row.get::<String, _>("state"), "awaiting_consent");

    database.cleanup().await;
}
