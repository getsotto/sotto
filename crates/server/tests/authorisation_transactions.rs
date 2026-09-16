//! Required database execution harness for server authorisation assurance.
//!
//! The cases in this binary deliberately run through the production router against Postgres.  A
//! normal local workspace test may skip when the opt-in is absent, but CI sets
//! `SOTTO_RUN_DB_TESTS=1`; in that mode a missing URL or an unreachable database is a failure.

use std::str::FromStr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    let required = std::env::var("SOTTO_RUN_DB_TESTS").as_deref() == Ok("1");
    if !required {
        eprintln!("skipping server assurance: set SOTTO_RUN_DB_TESTS=1 and DATABASE_URL");
        return None;
    }
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(error) => panic!("DATABASE_URL is required for server assurance: {error}"),
    };

    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "server assurance only accepts a dedicated loopback database, got {}",
        options.get_host()
    );
    let pool = db::connect(&url)
        .await
        .expect("connect to the server assurance database");
    db::migrate(&pool)
        .await
        .expect("migrate the server assurance database");
    Some(pool)
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        telemetry_ingest: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    };
    sotto_server::app(state)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    String::from_utf8(bytes.to_vec()).expect("response body is utf8")
}

async fn request(
    pool: &PgPool,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = app(pool.clone())
        .oneshot(request)
        .await
        .expect("router response");
    let status = response.status();
    (status, body_text(response).await)
}

async fn get(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "GET", uri, Some(token), None).await
}

async fn post(pool: &PgPool, token: &str, uri: &str, body: String) -> (StatusCode, String) {
    request(pool, "POST", uri, Some(token), Some(body)).await
}

async fn delete(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "DELETE", uri, Some(token), None).await
}

async fn fresh_session(pool: &PgPool, user_id: &str) -> String {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean user");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2)")
        .bind(user_id)
        .bind(format!("{user_id}-subject"))
        .execute(pool)
        .await
        .expect("insert user");
    session::issue(pool, user_id).await.expect("issue session")
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn org_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_org_key":"{}"}}"#,
        b64(b"org"),
        b64(b"org-key")
    )
}

fn member_body(user_id: &str, role: &str) -> String {
    format!(r#"{{"user_id":"{user_id}","role":"{role}"}}"#)
}

fn org_project_body(id: &str, org_id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","org_id":"{org_id}"}}"#,
        b64(b"project")
    )
}

fn project_body(id: &str) -> String {
    format!(r#"{{"id":"{id}","enc_name":"{}"}}"#, b64(b"project"))
}

fn env_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
        b64(b"env"),
        b64(b"vault-key")
    )
}

fn set_body(base: i64, secret_id: &str) -> String {
    format!(
        r#"{{"base_revision":{base},"changes":[{{"id":"{secret_id}","op":"set","version":1,"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
        b64(b"name"),
        b64(b"value"),
        b64(b"data-key")
    )
}

fn grant_body(user_id: &str, key: &[u8]) -> String {
    format!(
        r#"{{"user_id":"{user_id}","enc_vault_key":"{}"}}"#,
        b64(key)
    )
}

fn token_body() -> String {
    format!(
        r#"{{"name":"assurance","public_key":"{}","enc_vault_key":"{}"}}"#,
        b64(&[7; 32]),
        b64(b"machine-key")
    )
}

async fn seed_org_env(
    pool: &PgPool,
    suffix: &str,
    include_admin: bool,
) -> (String, String, String, String, Option<String>, String) {
    let org = format!("assure-{suffix}-org");
    let project = format!("assure-{suffix}-project");
    let env = format!("assure-{suffix}-env");
    let owner_id = format!("assure-{suffix}-owner");
    let member_id = format!("assure-{suffix}-member");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org)
        .execute(pool)
        .await
        .expect("clean organisation fixture");
    let owner = fresh_session(pool, &owner_id).await;
    let member = fresh_session(pool, &member_id).await;
    assert_eq!(
        post(pool, &owner, "/orgs", org_body(&org)).await.0,
        StatusCode::CREATED
    );
    let admin = if include_admin {
        let admin_id = format!("assure-{suffix}-admin");
        let token = fresh_session(pool, &admin_id).await;
        assert_eq!(
            post(
                pool,
                &owner,
                &format!("/orgs/{org}/members"),
                member_body(&admin_id, "admin")
            )
            .await
            .0,
            StatusCode::CREATED
        );
        Some(token)
    } else {
        None
    };
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/orgs/{org}/members"),
            member_body(&member_id, "member")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(pool, &owner, "/projects", org_project_body(&project, &org))
            .await
            .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/projects/{project}/environments"),
            env_body(&env)
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/environments/{env}/secrets"),
            set_body(0, &format!("{env}-secret"))
        )
        .await
        .0,
        StatusCode::OK
    );
    (owner, member, org, project, admin, env)
}

async fn wait_for_blocked(pool: &PgPool, label: &str, minimum: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
               AND cardinality(pg_blocking_pids(pid)) > 0",
        )
        .fetch_one(pool)
        .await
        .expect("inspect blocked server assurance sessions");
        if count >= minimum {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {minimum} blocked session(s) at {label}; observed {count}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn case_access_matrix(pool: &PgPool) {
    let (owner, member, org, project, admin, env) = seed_org_env(pool, "matrix", true).await;
    let admin = admin.expect("admin fixture");
    let outsider = fresh_session(pool, "assure-matrix-outsider").await;

    assert_eq!(
        get(pool, &member, &format!("/environments/{env}/secrets"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        post(
            pool,
            &member,
            "/projects",
            org_project_body("assure-matrix-member-project", &org)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            pool,
            &admin,
            "/projects",
            org_project_body("assure-matrix-admin-project", &org)
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        get(
            pool,
            &outsider,
            &format!("/projects/{project}/environments")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    let personal_owner = fresh_session(pool, "assure-matrix-personal-owner").await;
    let personal_other = fresh_session(pool, "assure-matrix-personal-other").await;
    assert_eq!(
        post(
            pool,
            &personal_owner,
            "/projects",
            project_body("assure-matrix-personal")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &personal_owner,
            "/projects/assure-matrix-personal/environments",
            env_body("assure-matrix-personal-env")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        get(
            pool,
            &personal_other,
            "/projects/assure-matrix-personal/environments"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(pool, &owner, &format!("/orgs/{org}/members")).await.0,
        StatusCode::OK
    );
}

async fn case_stale_authorisation(pool: &PgPool) {
    let (_owner, member, org, _project, _admin, env) =
        seed_org_env(pool, "stale-remove", false).await;
    let mut blocker = pool.begin().await.expect("begin removal blocker");
    sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(&org)
        .fetch_one(&mut *blocker)
        .await
        .expect("lock organisation");
    let request_pool = pool.clone();
    let request_member = member.clone();
    let request_env = env.clone();
    let waiting = tokio::spawn(async move {
        post(
            &request_pool,
            &request_member,
            &format!("/environments/{request_env}/secrets"),
            set_body(1, "assure-stale-remove-late"),
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(
        !waiting.is_finished(),
        "stale removal request completed before the lock checkpoint"
    );
    wait_for_blocked(pool, "stale member removal", 1).await;
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1 AND user_id = $2")
        .bind(&org)
        .bind("assure-stale-remove-member")
        .execute(&mut *blocker)
        .await
        .expect("remove member while request waits");
    blocker.commit().await.expect("commit member removal");
    assert_eq!(
        waiting.await.expect("join stale removal").0,
        StatusCode::NOT_FOUND
    );

    let (owner, _member, org, _project, admin, _env) =
        seed_org_env(pool, "stale-admin", true).await;
    let admin = admin.expect("admin fixture");
    let mut blocker = pool.begin().await.expect("begin demotion blocker");
    sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(&org)
        .fetch_one(&mut *blocker)
        .await
        .expect("lock organisation");
    let request_pool = pool.clone();
    let request_admin = admin.clone();
    let request_org = org.clone();
    let waiting = tokio::spawn(async move {
        post(
            &request_pool,
            &request_admin,
            "/projects",
            org_project_body("assure-stale-admin-late", &request_org),
        )
        .await
    });
    wait_for_blocked(pool, "stale admin demotion", 1).await;
    sqlx::query(
        "UPDATE organization_memberships SET role = 'member' WHERE org_id = $1 AND user_id = $2",
    )
    .bind(&org)
    .bind("assure-stale-admin-admin")
    .execute(&mut *blocker)
    .await
    .expect("demote admin while request waits");
    blocker.commit().await.expect("commit admin demotion");
    assert_eq!(
        waiting.await.expect("join stale demotion").0,
        StatusCode::FORBIDDEN
    );
    let _ = owner;
}

async fn case_lifecycle_recheck(pool: &PgPool) {
    for (state, expected) in [
        ("deleting", StatusCode::CONFLICT),
        ("deleted", StatusCode::NOT_FOUND),
    ] {
        let (owner, _member, org, _project, _admin, env) =
            seed_org_env(pool, &format!("lifecycle-{state}"), false).await;
        let mut blocker = pool.begin().await.expect("begin lifecycle blocker");
        sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(&org)
            .fetch_one(&mut *blocker)
            .await
            .expect("lock organisation");
        let request_pool = pool.clone();
        let request_owner = owner.clone();
        let request_env = env.clone();
        let waiting = tokio::spawn(async move {
            post(
                &request_pool,
                &request_owner,
                &format!("/environments/{request_env}/secrets"),
                set_body(1, "assure-lifecycle-late"),
            )
            .await
        });
        wait_for_blocked(pool, &format!("lifecycle {state}"), 1).await;
        if state == "deleted" {
            sqlx::query(
                "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), enc_name = NULL, created_by = NULL, tier = 'free', trial_ends_at = NULL WHERE id = $1",
            )
            .bind(&org)
            .execute(&mut *blocker)
            .await
            .expect("mark organisation deleted");
        } else {
            sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
                .bind(&org)
                .execute(&mut *blocker)
                .await
                .expect("mark organisation deleting");
        }
        blocker.commit().await.expect("commit lifecycle transition");
        assert_eq!(waiting.await.expect("join lifecycle request").0, expected);
    }
}

async fn case_concurrent_batches(pool: &PgPool) {
    let owner = fresh_session(pool, "assure-race-owner").await;
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects",
            project_body("assure-race-project")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects/assure-race-project/environments",
            env_body("assure-race-env")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let mut blocker = pool.begin().await.expect("begin revision blocker");
    sqlx::query("SELECT revision FROM environments WHERE id = $1 FOR UPDATE")
        .bind("assure-race-env")
        .fetch_one(&mut *blocker)
        .await
        .expect("lock environment");
    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let token_a = owner.clone();
    let token_b = owner.clone();
    let first = tokio::spawn(async move {
        post(
            &pool_a,
            &token_a,
            "/environments/assure-race-env/secrets",
            set_body(0, "assure-race-a"),
        )
        .await
    });
    let second = tokio::spawn(async move {
        post(
            &pool_b,
            &token_b,
            "/environments/assure-race-env/secrets",
            set_body(0, "assure-race-b"),
        )
        .await
    });
    wait_for_blocked(pool, "competing batch revisions", 2).await;
    blocker.commit().await.expect("release revision blocker");
    let first = first.await.expect("join first batch").0;
    let second = second.await.expect("join second batch").0;
    assert!(
        [first, second].contains(&StatusCode::OK)
            && [first, second].contains(&StatusCode::PRECONDITION_FAILED),
        "competing batches must produce one success and one precondition failure: {first} {second}"
    );
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1")
        .bind("assure-race-env")
        .fetch_one(pool)
        .await
        .expect("read winning revision");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM secrets WHERE env_id = $1")
        .bind("assure-race-env")
        .fetch_one(pool)
        .await
        .expect("count winning secrets");
    assert_eq!(revision, 1);
    assert_eq!(count, 1);
}

async fn case_removal_and_atomic_batch(pool: &PgPool) {
    let (owner, _member, org, _project, admin, env) = seed_org_env(pool, "removal", true).await;
    let admin = admin.expect("admin fixture");
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/environments/{env}/grants"),
            grant_body("assure-removal-admin", b"admin-grant")
        )
        .await
        .0,
        StatusCode::OK
    );
    let (token_status, token_body_json) = post(
        pool,
        &admin,
        &format!("/environments/{env}/tokens"),
        token_body(),
    )
    .await;
    assert_eq!(token_status, StatusCode::CREATED);
    let token_json = serde_json::from_str::<Value>(&token_body_json).expect("token json");
    let token_id = token_json["token_id"]
        .as_str()
        .expect("token id")
        .to_owned();
    let raw_token = token_json["token"].as_str().expect("raw token").to_owned();
    assert_eq!(
        delete(
            pool,
            &owner,
            &format!("/orgs/{org}/members/assure-removal-admin")
        )
        .await
        .0,
        StatusCode::OK
    );
    let grant_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(&env)
    .bind("assure-removal-admin")
    .fetch_one(pool)
    .await
    .expect("count removed grants");
    let revoked: Option<String> =
        sqlx::query_scalar("SELECT revoked_at::text FROM machine_tokens WHERE id = $1")
            .bind(&token_id)
            .fetch_one(pool)
            .await
            .expect("read revoked token");
    assert_eq!(grant_count, 0);
    assert!(revoked.is_some());
    assert_eq!(
        get(pool, &raw_token, "/machine/grant").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/orgs/{org}/members"),
            member_body("assure-removal-admin", "member")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        get(pool, &admin, &format!("/environments/{env}/grant"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let owner = fresh_session(pool, "assure-atomic-owner").await;
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects",
            project_body("assure-atomic-project")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects/assure-atomic-project/environments",
            env_body("assure-atomic-env")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let malformed = format!(
        r#"{{"base_revision":0,"changes":[{{"id":"assure-atomic-first","op":"set","version":1,"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}},{{"id":"bad/id","op":"set","version":1,"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
        b64(b"name"),
        b64(b"value"),
        b64(b"key"),
        b64(b"name"),
        b64(b"value"),
        b64(b"key")
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/environments/assure-atomic-env/secrets",
            malformed
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1")
        .bind("assure-atomic-env")
        .fetch_one(pool)
        .await
        .expect("read atomic revision");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM secrets WHERE env_id = $1")
        .bind("assure-atomic-env")
        .fetch_one(pool)
        .await
        .expect("read atomic secrets");
    assert_eq!(revision, 0);
    assert_eq!(count, 0);
}

async fn case_sequential_rechecks(pool: &PgPool) {
    let (_owner, member, org, _project, _admin, env) =
        seed_org_env(pool, "sequential", false).await;
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1 AND user_id = $2")
        .bind(&org)
        .bind("assure-sequential-member")
        .execute(pool)
        .await
        .expect("remove sequential member");
    assert_eq!(
        post(
            pool,
            &member,
            &format!("/environments/{env}/secrets"),
            set_body(1, "assure-sequential-late")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    let (owner, _member, org, project, admin, _env) =
        seed_org_env(pool, "sequential-admin", true).await;
    let admin = admin.expect("admin fixture");
    sqlx::query(
        "UPDATE organization_memberships SET role = 'member' WHERE org_id = $1 AND user_id = $2",
    )
    .bind(&org)
    .bind("assure-sequential-admin-admin")
    .execute(pool)
    .await
    .expect("demote sequential admin");
    assert_eq!(
        post(
            pool,
            &admin,
            "/projects",
            org_project_body("assure-sequential-admin-late", &org)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(pool, &owner, &format!("/projects/{project}/environments"))
            .await
            .0,
        StatusCode::OK
    );
}

async fn case_lifecycle_and_revision_conflicts(pool: &PgPool) {
    let (owner, _member, org, _project, _admin, env) =
        seed_org_env(pool, "lifecycle-sequential", false).await;
    sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
        .bind(&org)
        .execute(pool)
        .await
        .expect("mark organisation deleting");
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/environments/{env}/secrets"),
            set_body(1, "assure-lifecycle-write")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), enc_name = NULL, created_by = NULL, tier = 'free', trial_ends_at = NULL WHERE id = $1",
    )
    .bind(&org)
    .execute(pool)
    .await
    .expect("mark organisation deleted");
    assert_eq!(
        post(
            pool,
            &owner,
            &format!("/environments/{env}/secrets"),
            set_body(1, "assure-deleted-write")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    let owner = fresh_session(pool, "assure-revision-owner").await;
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects",
            project_body("assure-revision-project")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/projects/assure-revision-project/environments",
            env_body("assure-revision-env")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/environments/assure-revision-env/secrets",
            set_body(0, "assure-revision-first")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post(
            pool,
            &owner,
            "/environments/assure-revision-env/secrets",
            set_body(0, "assure-revision-stale")
        )
        .await
        .0,
        StatusCode::PRECONDITION_FAILED
    );
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM environments WHERE id = $1")
        .bind("assure-revision-env")
        .fetch_one(pool)
        .await
        .expect("read revision");
    assert_eq!(revision, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_assurance_executes_against_the_required_database() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    tokio::time::timeout(Duration::from_secs(30), async {
        case_access_matrix(&pool).await;
        case_stale_authorisation(&pool).await;
        case_lifecycle_recheck(&pool).await;
        case_concurrent_batches(&pool).await;
        case_sequential_rechecks(&pool).await;
        case_removal_and_atomic_batch(&pool).await;
        case_lifecycle_and_revision_conflicts(&pool).await;
    })
    .await
    .expect("server assurance scenario suite timed out");

    // Leading newline: the harness prints `test ... ... ` without one, so without this
    // the marker shares that line and the CI completion grep cannot match it.
    println!("\nSERVER_ASSURANCE_DONE 7");
}
