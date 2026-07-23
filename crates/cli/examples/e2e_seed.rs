//! E2E fixture seed for the funnel regression suite (issue #64).
//!
//! Creates two real accounts (an owner and an invitee) with genuine crypto material, an org, a
//! project/environment with a secret, and a pending invite - so the Playwright suite can start
//! at "login" instead of reconstructing account state itself. Mirrors
//! `crates/cli/tests/e2e.rs::env_sharing_and_removal_end_to_end_over_http` exactly (same
//! library calls, same direct-session-mint pattern for a second user), except it seeds a
//! Postgres a *separately running* server process already uses, rather than spinning up its own
//! in-process server.
//!
//! Not part of any shipped binary: an `examples/` target, using `[dev-dependencies]` only
//! (`sotto-server`, for direct DB/session access - never linked into the real `sotto` CLI).
//!
//! Identities are fixed, not random, so the E2E server's mock OAuth provider
//! (`--features e2e-mock-oauth`) resolves the SAME users when Playwright's browser logs in:
//! `MockOAuth::exchange_code(code)` sets the OAuth subject to `code` verbatim, so a browser
//! login with `code=e2e-owner` upserts onto the row this script already created.
//!
//! Usage:
//!   DATABASE_URL=postgres://... cargo run -p sotto-cli --example e2e_seed -- <server-url>
//!
//! Prints one JSON object to stdout for the Playwright suite to read.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::Row;

use sotto_cli::config::Config;
use sotto_cli::keychain::MemoryKeychain;
use sotto_cli::remote::{self, HttpClient, SyncApi};
use sotto_cli::session;
use sotto_cli::store::Store;
use sotto_cli::vault::Vault;

const OWNER_SUBJECT: &str = "e2e-owner";
const OWNER_PASSWORD: &[u8] = b"e2e-owner-password";
const INVITEE_SUBJECT: &str = "e2e-invitee";
const INVITEE_EMAIL: &str = "e2e-invitee@e2e.sotto.test";
const INVITEE_PASSWORD: &[u8] = b"e2e-invitee-password";
const PROJECT_NAME: &str = "e2e-project";
const SECRET_NAME: &str = "DATABASE_URL";
const SECRET_VALUE: &[u8] = b"postgres://e2e-fixture-value";
const ORG_NAME: &str = "E2E Org";
const TTL: Duration = Duration::from_secs(3600);

#[derive(serde::Serialize)]
struct Fixture {
    owner_login_code: String,
    owner_password: String,
    owner_secret_key: String,
    invitee_login_code: String,
    invitee_user_id: String,
    invitee_email: String,
    org_id: String,
    project_name: String,
    secret_name: String,
    secret_value: String,
}

fn main() {
    let server_url = std::env::args()
        .nth(1)
        .expect("usage: e2e_seed <server-url>");
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Mint sessions for both users directly against Postgres - same shortcut
    // `crates/cli/tests/e2e.rs` already uses for its second test user, since the seed script's
    // job is to establish state, not to exercise the OAuth handshake itself (Playwright's
    // browser does that, separately, via the mock provider).
    let (owner_token, invitee_token, invitee_id) = rt.block_on(async {
        let pool = sotto_server::db::connect(&database_url)
            .await
            .expect("connect to postgres");
        sotto_server::db::migrate(&pool).await.expect("migrate");

        let owner_id = insert_user(&pool, OWNER_SUBJECT, None).await;
        let invitee_id = insert_user(&pool, INVITEE_SUBJECT, Some(INVITEE_EMAIL)).await;

        let owner_token = sotto_server::auth::session::issue(&pool, &owner_id)
            .await
            .expect("issue owner session");
        let invitee_token = sotto_server::auth::session::issue(&pool, &invitee_id)
            .await
            .expect("issue invitee session");
        (owner_token, invitee_token, invitee_id)
    });

    // --- Owner: local identity, an org, a project + secret, pushed to the server. ---
    let owner_client = HttpClient::new(server_url.clone(), owner_token);
    let store_owner = Store::open_in_memory().expect("open owner store");
    let kc_owner = MemoryKeychain::default();
    let kit = session::init(&store_owner, &kc_owner, OWNER_PASSWORD, TTL).expect("owner init");
    let master_key_owner = session::current_master_key(&kc_owner)
        .expect("owner master key")
        .expect("owner unlocked");
    let keypair_owner =
        session::account_keypair(&store_owner, &master_key_owner).expect("owner keypair");
    let master_owner = *master_key_owner.as_bytes();

    // Idempotent rerun: reuse a prior run's org rather than accumulating a fresh one every time
    // (a real problem for local dev, where Postgres persists across runs - the Playwright suite
    // matches on the org's name, and a duplicate would make that match ambiguous).
    let existing_org = remote::team::list_orgs(&owner_client, &keypair_owner)
        .expect("list orgs")
        .into_iter()
        .find(|o| o.name == ORG_NAME)
        .map(|o| o.id);
    let org_id = match existing_org {
        Some(id) => id,
        None => {
            remote::team::create_org(&owner_client, &keypair_owner, ORG_NAME).expect("create org")
        }
    };
    let project =
        Vault::create_project(&store_owner, &keypair_owner, PROJECT_NAME).expect("create project");
    let config = Config {
        project_id: project.id.clone(),
        project: PROJECT_NAME.into(),
        environment: "dev".into(),
        org_id: Some(org_id.clone()),
    };
    Vault::open(&store_owner, &keypair_owner, &project.id, "dev")
        .expect("open vault")
        .set(SECRET_NAME, SECRET_VALUE)
        .expect("set secret");
    remote::sync::push(&owner_client, &store_owner, &master_owner, &config).expect("push");

    // --- Invitee: their own local identity, account material pushed so a public key is on file
    // (required before `invite` can seal them a working org-key grant). ---
    let invitee_client = HttpClient::new(server_url.clone(), invitee_token);
    let store_invitee = Store::open_in_memory().expect("open invitee store");
    let kc_invitee = MemoryKeychain::default();
    session::init(&store_invitee, &kc_invitee, INVITEE_PASSWORD, TTL).expect("invitee init");
    let material = sotto_cli::account::material(&store_invitee).expect("invitee material");
    // Idempotent rerun: a prior run may have already uploaded this fixed-subject invitee's
    // account (with different, since-discarded key material - fine, nothing in the Playwright
    // suite ever unlocks the invitee's account; it only needs SOME public key on file so the
    // owner's live invite grants cleanly). Same conflict-is-fine pattern as `ensure_account` in
    // `crates/cli/src/remote/sync.rs`.
    match invitee_client.put_account(&remote::api::AccountBundle {
        public_key: STANDARD.encode(&material.public_key),
        enc_private_keys: STANDARD.encode(&material.enc_private_keys),
        kdf_params: STANDARD.encode(&material.kdf_params),
        recovery_blob: STANDARD.encode(&material.recovery_blob),
    }) {
        Ok(()) | Err(sotto_cli::error::Error::Conflict(_)) => {}
        Err(e) => panic!("upload invitee account: {e}"),
    }

    // The invite itself is NOT performed here: it's a funnel step Playwright drives live, by
    // typing `invitee_email` into TeamPanel's "Invite by email" field. This only prepares the
    // invitee to BE inviteable - their public key is on file, so the live invite grants cleanly.

    let fixture = Fixture {
        owner_login_code: OWNER_SUBJECT.into(),
        owner_password: String::from_utf8(OWNER_PASSWORD.to_vec()).unwrap(),
        owner_secret_key: kit.secret_key,
        invitee_login_code: INVITEE_SUBJECT.into(),
        invitee_user_id: invitee_id,
        invitee_email: INVITEE_EMAIL.into(),
        org_id,
        project_name: PROJECT_NAME.into(),
        secret_name: SECRET_NAME.into(),
        secret_value: String::from_utf8(SECRET_VALUE.to_vec()).unwrap(),
    };
    println!(
        "{}",
        serde_json::to_string(&fixture).expect("serialize fixture")
    );
}

/// Insert (or reuse, on a rerun) a fixed-subject test user directly - the same shortcut
/// `crates/cli/tests/e2e.rs` uses for its second user. Fixed, not random, subjects: the E2E
/// server's mock OAuth provider must resolve to this exact row when Playwright logs in with the
/// matching code.
async fn insert_user(pool: &sqlx::PgPool, subject: &str, email: Option<&str>) -> String {
    let id = format!("e2e-{subject}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, 'github', $2, $3) \
         ON CONFLICT (oauth_provider, oauth_subject) DO UPDATE SET email = EXCLUDED.email \
         RETURNING id",
    )
    .bind(&id)
    .bind(subject)
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("insert test user")
    .get::<String, _>(0)
}
