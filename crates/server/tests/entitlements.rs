//! Entitlements (M6 PR5): the 14-day Team trial, free-tier quotas, and the Team feature gate.
//!
//! New orgs trial the Team tier; expiry drops them to the (deliberately tight) free limits;
//! a manual upgrade lifts everything. Enforcement is creation-time + feature-gate only — an
//! expired trial never blocks reading or syncing what a team already has. DB-gated like the rest.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
    };
    Router::new()
        .merge(sotto_server::org::router())
        .merge(sotto_server::audit::router())
        .merge(sotto_server::entitlements::router())
        .merge(sotto_server::sync::router())
        .with_state(state)
}

async fn reset_orgs(pool: &PgPool, orgs: &[&str]) {
    for org in orgs {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(pool)
            .await
            .expect("reset org");
    }
}

async fn fresh_session(pool: &PgPool, user_id: &str, subject: &str) -> String {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("pre-clean");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2)")
        .bind(user_id)
        .bind(subject)
        .execute(pool)
        .await
        .expect("insert user");
    session::issue(pool, user_id).await.expect("issue")
}

async fn ensure_user(pool: &PgPool, user_id: &str, subject: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .bind(subject)
    .execute(pool)
    .await
    .expect("ensure user");
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

async fn send(
    pool: &PgPool,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b))
            .expect("req"),
        None => builder.body(Body::empty()).expect("req"),
    };
    let resp = app(pool.clone()).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    (status, body_text(resp).await)
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn org_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_org_key":"{}"}}"#,
        b64(b"org"),
        b64(b"ok"),
    )
}

fn member_body(user_id: &str) -> String {
    format!(r#"{{"user_id":"{user_id}","role":"member"}}"#)
}

fn org_project_body(id: &str, org_id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","org_id":"{org_id}"}}"#,
        b64(b"p")
    )
}

/// Force an org's trial into the past, dropping it to free-tier enforcement.
async fn expire_trial(pool: &PgPool, org_id: &str) {
    sqlx::query("UPDATE organizations SET trial_ends_at = now() - interval '1 day' WHERE id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("expire trial");
}

#[tokio::test]
async fn trial_grants_team_then_expiry_enforces_free_limits() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let o = "ent-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "ent-owner", "ent-owner-s").await;
    for i in 1..=4 {
        ensure_user(&pool, &format!("ent-m{i}"), &format!("ent-m{i}-s")).await;
    }
    send(&pool, "POST", "/orgs", &owner, Some(org_body(o))).await;

    // On trial: effectively Team — no limits reported, audit readable, many members fine.
    let (status, body) = send(
        &pool,
        "GET",
        &format!("/orgs/{o}/entitlements"),
        &owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"tier\":\"free\""));
    assert!(body.contains("\"effective_tier\":\"team\""));
    assert!(body.contains("\"limits\":null"));
    assert_eq!(
        send(&pool, "GET", &format!("/orgs/{o}/audit"), &owner, None)
            .await
            .0,
        StatusCode::OK
    );
    for i in 1..=3 {
        assert_eq!(
            send(
                &pool,
                "POST",
                &format!("/orgs/{o}/members"),
                &owner,
                Some(member_body(&format!("ent-m{i}")))
            )
            .await
            .0,
            StatusCode::CREATED,
            "trial allows member {i}"
        );
    }
    assert_eq!(
        send(
            &pool,
            "POST",
            "/projects",
            &owner,
            Some(org_project_body("ent-p1", o))
        )
        .await
        .0,
        StatusCode::CREATED
    );

    // Trial over: the org drops to free — audit gated, both quotas enforced with 402s.
    expire_trial(&pool, o).await;
    let (_, body) = send(
        &pool,
        "GET",
        &format!("/orgs/{o}/entitlements"),
        &owner,
        None,
    )
    .await;
    assert!(body.contains("\"effective_tier\":\"free\""));
    assert!(body.contains("\"max_members\":3"));
    assert_eq!(
        send(&pool, "GET", &format!("/orgs/{o}/audit"), &owner, None)
            .await
            .0,
        StatusCode::PAYMENT_REQUIRED
    );
    // Already at 4 members (owner + 3): adding another is blocked. Removal still works (never
    // gate shrinking), but re-adding past the limit stays blocked.
    assert_eq!(
        send(
            &pool,
            "POST",
            &format!("/orgs/{o}/members"),
            &owner,
            Some(member_body("ent-m4"))
        )
        .await
        .0,
        StatusCode::PAYMENT_REQUIRED
    );
    // A second org project is blocked; re-creating the existing one (the idempotent push path)
    // still succeeds so syncing never breaks.
    assert_eq!(
        send(
            &pool,
            "POST",
            "/projects",
            &owner,
            Some(org_project_body("ent-p2", o))
        )
        .await
        .0,
        StatusCode::PAYMENT_REQUIRED
    );
    assert_eq!(
        send(
            &pool,
            "POST",
            "/projects",
            &owner,
            Some(org_project_body("ent-p1", o))
        )
        .await
        .0,
        StatusCode::OK,
        "idempotent re-create of an existing project must keep working at the limit"
    );

    // Manual upgrade (what a future Stripe webhook will do): everything unblocks.
    sqlx::query("UPDATE organizations SET tier = 'team' WHERE id = $1")
        .bind(o)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = send(
        &pool,
        "GET",
        &format!("/orgs/{o}/entitlements"),
        &owner,
        None,
    )
    .await;
    assert!(body.contains("\"tier\":\"team\""));
    assert!(body.contains("\"effective_tier\":\"team\""));
    assert_eq!(
        send(&pool, "GET", &format!("/orgs/{o}/audit"), &owner, None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &pool,
            "POST",
            &format!("/orgs/{o}/members"),
            &owner,
            Some(member_body("ent-m4"))
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &pool,
            "POST",
            "/projects",
            &owner,
            Some(org_project_body("ent-p2", o))
        )
        .await
        .0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn entitlements_are_member_visible_only() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let o = "ent-vis-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "ent-vis-owner", "ent-vis-owner-s").await;
    let outsider = fresh_session(&pool, "ent-vis-out", "ent-vis-out-s").await;
    send(&pool, "POST", "/orgs", &owner, Some(org_body(o))).await;

    assert_eq!(
        send(
            &pool,
            "GET",
            &format!("/orgs/{o}/entitlements"),
            &owner,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &pool,
            "GET",
            &format!("/orgs/{o}/entitlements"),
            &outsider,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "non-members don't learn the org exists"
    );
}
