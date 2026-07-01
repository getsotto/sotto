//! End-to-end test over real HTTP + Postgres.
//!
//! Device A initializes, sets a secret, and pushes; device B (a fresh store) reconstructs its
//! identity + environment from the server using the Emergency Kit, pulls, and decrypts the same
//! value. This exercises the real wire — the CLI's blocking HTTP client against the actual axum
//! server and Postgres — that the mock-based engine tests stand in for.
//!
//! DB-gated: runs only when `SOTTO_RUN_DB_TESTS=1` and `DATABASE_URL` points at a local Postgres
//! (it applies migrations and writes rows, so it must never touch a non-local database). A session
//! is minted directly (the GitHub OAuth handshake is covered by the server's own tests); everything
//! past authentication is the genuine flow.
//!
//! Test data uses fresh UUID ids per run, so leftover rows never collide; CI runs against an
//! ephemeral Postgres, so they don't accumulate.

use std::str::FromStr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sqlx::postgres::PgConnectOptions;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;

use sotto_cli::config::Config;
use sotto_cli::keychain::MemoryKeychain;
use sotto_cli::remote::{self, HttpClient, SyncApi};
use sotto_cli::session;
use sotto_cli::store::Store;
use sotto_cli::vault::Vault;

/// The real server on a background thread (its own runtime), plus a valid session for it.
struct TestServer {
    url: String,
    token: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(database_url: &str) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel::<(u16, String)>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let database_url = database_url.to_string();

        let handle = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            rt.block_on(async move {
                let pool = sotto_server::db::connect(&database_url).await.expect("connect");
                sotto_server::db::migrate(&pool).await.expect("migrate");

                // Mint a user + session directly, skipping the OAuth browser flow.
                let user_id = format!("e2e-user-{}", uuid::Uuid::new_v4());
                sqlx::query(
                    "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2)",
                )
                .bind(&user_id)
                .bind(&user_id)
                .execute(&pool)
                .await
                .expect("insert user");
                let token = sotto_server::auth::session::issue(&pool, &user_id)
                    .await
                    .expect("issue session");

                let state = sotto_server::state::AppState {
                    pool,
                    oauth: None,
                    oauth_config: None,
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let port = listener.local_addr().expect("addr").port();
                ready_tx.send((port, token)).expect("send ready");

                axum::serve(listener, sotto_server::app(state))
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        });

        let (port, token) = ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("server failed to start");
        TestServer {
            url: format!("http://127.0.0.1:{port}"),
            token,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Gate destructive DB tests behind an explicit opt-in and a local host, so a stray `DATABASE_URL`
/// (e.g. one a developer exported for something else) can never run migrations/inserts against it.
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

#[test]
fn new_device_end_to_end_over_http() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }
    let server = TestServer::start(&database_url);
    let ttl = Duration::from_secs(3600);
    let client = HttpClient::new(server.url.clone(), server.token.clone());

    // --- Device A: init, set a secret, push. ---
    let store_a = Store::open_in_memory().unwrap();
    let kc_a = MemoryKeychain::default();
    let kit = session::init(&store_a, &kc_a, b"pw", ttl).unwrap();
    let master_key_a = session::current_master_key(&kc_a).unwrap().unwrap();
    let keypair_a = session::account_keypair(&store_a, &master_key_a).unwrap();
    let master_a = *master_key_a.as_bytes();
    let project = Vault::create_project(&store_a, &keypair_a, "acme").unwrap();
    let config = Config {
        project_id: project.id.clone(),
        project: "acme".into(),
        environment: "dev".into(),
        org_id: None,
    };
    Vault::open(&store_a, &keypair_a, &project.id, "dev")
        .unwrap()
        .set("DATABASE_URL", b"postgres://prod")
        .unwrap();
    remote::sync::push(&client, &store_a, &master_a, &config).unwrap();

    // --- Device B: reconstruct from the server + Emergency Kit, then pull. ---
    let store_b = Store::open_in_memory().unwrap();
    let kc_b = MemoryKeychain::default();
    let bundle = remote::SyncApi::get_account(&client).unwrap().unwrap();
    let secret_key = sotto_core::format::decode_key("SK", 1, &kit.secret_key).unwrap();
    remote::sync::restore_account(&store_b, &kc_b, &bundle, &secret_key, b"pw", ttl).unwrap();
    let master_key_b = session::current_master_key(&kc_b).unwrap().unwrap();
    let keypair_b = session::account_keypair(&store_b, &master_key_b).unwrap();
    let master_b = *master_key_b.as_bytes();
    assert_eq!(master_a, master_b, "reconstructed master key matches");

    remote::sync::pull_environments(&client, &store_b, &master_b, &config).unwrap();
    remote::sync::pull(&client, &store_b, &config).unwrap();

    let value = Vault::open(&store_b, &keypair_b, &project.id, "dev")
        .unwrap()
        .get("DATABASE_URL")
        .unwrap();
    assert_eq!(value, b"postgres://prod");
}

#[test]
fn share_link_end_to_end_over_http() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }
    let server = TestServer::start(&database_url);
    let client = HttpClient::new(server.url.clone(), server.token.clone());

    // Create a one-time share link, exactly as `sotto share` does.
    let opts = remote::share::ShareOptions {
        max_views: 1,
        ttl_seconds: Some(3600),
        passphrase: None,
    };
    let link = remote::share::create(&client, "https://app.example", b"the-secret", &opts).unwrap();

    // Parse the recipient link: <web>/s/<token>#<fragment-key>.
    let (base, fragment) = link.split_once('#').unwrap();
    let token = base.rsplit('/').next().unwrap();
    let fragment_key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(fragment)
        .unwrap()
        .try_into()
        .unwrap();

    // The recipient path (what the web page does): public fetch → base64-decode → decrypt.
    let http = reqwest::blocking::Client::new();
    let resp = http
        .get(format!("{}/shares/{token}", server.url))
        .send()
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().unwrap();
    let enc_blob = STANDARD.decode(body["enc_blob"].as_str().unwrap()).unwrap();
    let plaintext = sotto_core::share::open(&fragment_key, &enc_blob).unwrap();
    assert_eq!(plaintext, b"the-secret");

    // One-time: the second fetch is burned.
    let resp2 = http
        .get(format!("{}/shares/{token}", server.url))
        .send()
        .unwrap();
    assert_eq!(resp2.status(), reqwest::StatusCode::NOT_FOUND);
}

/// The full team-sharing loop over real HTTP: Alice creates an org + org-owned project, invites Bob
/// by email, and shares an environment; Bob (a distinct user + session) fetches his grant, clones
/// the environment, and decrypts the same secret. Exercises the CLI's org/invite/grant HTTP methods
/// and the cross-user crypto against the real server + Postgres.
#[test]
fn env_sharing_end_to_end_over_http() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }
    let server = TestServer::start(&database_url);
    let ttl = Duration::from_secs(3600);

    // --- Alice: create an org, an org-owned project with a secret, and push. ---
    let alice = HttpClient::new(server.url.clone(), server.token.clone());
    let store_a = Store::open_in_memory().unwrap();
    let kc_a = MemoryKeychain::default();
    session::init(&store_a, &kc_a, b"pw", ttl).unwrap();
    let master_key_a = session::current_master_key(&kc_a).unwrap().unwrap();
    let keypair_a = session::account_keypair(&store_a, &master_key_a).unwrap();
    let master_a = *master_key_a.as_bytes();

    let org_id = remote::team::create_org(&alice, &master_a, "acme-team").unwrap();
    let project = Vault::create_project(&store_a, &keypair_a, "acme").unwrap();
    let config = Config {
        project_id: project.id.clone(),
        project: "acme".into(),
        environment: "dev".into(),
        org_id: Some(org_id.clone()),
    };
    Vault::open(&store_a, &keypair_a, &project.id, "dev")
        .unwrap()
        .set("API_KEY", b"s3cr3t")
        .unwrap();
    remote::sync::push(&alice, &store_a, &master_a, &config).unwrap();

    // --- Bob: a second user, with a session + email, on the same server. ---
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (bob_token, bob_email) = rt.block_on(async {
        let pool = sotto_server::db::connect(&database_url).await.unwrap();
        let bob_id = format!("e2e-bob-{}", uuid::Uuid::new_v4());
        let bob_email = format!("{bob_id}@example.test");
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject, email) \
             VALUES ($1, 'github', $1, $2)",
        )
        .bind(&bob_id)
        .bind(&bob_email)
        .execute(&pool)
        .await
        .expect("insert bob");
        let token = sotto_server::auth::session::issue(&pool, &bob_id)
            .await
            .expect("issue bob session");
        (token, bob_email)
    });
    let bob = HttpClient::new(server.url.clone(), bob_token);

    // Bob sets up his device and uploads his account material, so his public key is on the server.
    let store_b = Store::open_in_memory().unwrap();
    let kc_b = MemoryKeychain::default();
    session::init(&store_b, &kc_b, b"pwB", ttl).unwrap();
    let master_key_b = session::current_master_key(&kc_b).unwrap().unwrap();
    let keypair_b = session::account_keypair(&store_b, &master_key_b).unwrap();
    let m = sotto_cli::account::material(&store_b).unwrap();
    bob.put_account(&remote::api::AccountBundle {
        public_key: STANDARD.encode(&m.public_key),
        enc_private_keys: STANDARD.encode(&m.enc_private_keys),
        kdf_params: STANDARD.encode(&m.kdf_params),
        recovery_blob: STANDARD.encode(&m.recovery_blob),
    })
    .unwrap();

    // --- Alice invites Bob by email and shares the dev environment with him. ---
    let invited = remote::team::invite(&alice, &org_id, &bob_email).unwrap();
    assert!(invited.public_key.is_some(), "Bob's key should be on the server");
    let env_id =
        remote::team::share_env(&alice, &store_a, &keypair_a, &org_id, &invited.user_id, &config)
            .unwrap();

    // --- Bob clones the shared environment and decrypts the same secret. ---
    let bob_config = remote::team::clone_env(
        &bob,
        &store_b,
        &keypair_b,
        &project.id,
        &env_id,
        "acme",
        "dev",
        Some(&org_id),
    )
    .unwrap();
    let value = Vault::open(
        &store_b,
        &keypair_b,
        &bob_config.project_id,
        &bob_config.environment,
    )
    .unwrap()
    .get("API_KEY")
    .unwrap();
    assert_eq!(value, b"s3cr3t");
}
