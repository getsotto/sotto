//! The Sotto sync / API backend. See `docs/DATA-MODEL.md`. M0 is a health-check stub; the
//! real REST surface (full-snapshot sync, versioned writes, grants, rotation) lands in M3.

use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    let app = Router::new().route("/health", get(|| async { "ok" }));

    let addr = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind listener");
    println!("sotto-server listening on http://{addr} (M3 stub)");

    axum::serve(listener, app).await.expect("serve");
}
