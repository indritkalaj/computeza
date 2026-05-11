//! End-to-end test: wire a real SqliteStore in front of axum, persist a
//! spec + status under `postgres-instance/primary`, then hit /status and
//! /resource/postgres-instance/primary and assert the UI surfaces what
//! was persisted.
//!
//! This is the closest thing to a black-box product test the workspace
//! has -- it exercises the full architecture loop (Store -> AppState ->
//! handler -> renderer) without spinning up an actual PostgreSQL.

use std::net::{Ipv4Addr, SocketAddr};

use computeza_state::{ResourceKey, SqliteStore, Store};
use computeza_ui_server::{router_with_state, AppState};
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn status_and_resource_pages_surface_persisted_state() {
    // Open an in-process SQLite db and persist one resource.
    let store = SqliteStore::open(":memory:")
        .await
        .expect("open in-memory SqliteStore");

    let key = ResourceKey::cluster_scoped("postgres-instance", "primary");
    let spec = json!({
        "endpoint": { "host": "localhost", "port": 5432 },
        "databases": [{ "name": "analytics" }],
    });
    store
        .save(&key, &spec, None)
        .await
        .expect("save initial spec");

    let status = json!({
        "server_version": "PostgreSQL 17.2 (Ubuntu) on x86_64",
        "databases": ["analytics"],
        "last_observed_at": "2026-05-11T08:00:00Z",
        "last_observe_failed": false,
    });
    store
        .put_status(&key, &status)
        .await
        .expect("put_status snapshot");

    // Spawn the server with the store attached.
    let state = AppState::with_store(store);
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind to free local port");
    let addr = listener.local_addr().expect("read bound addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, router_with_state(state))
            .await
            .expect("axum::serve")
    });

    let client = reqwest::Client::builder().build().expect("reqwest client");

    // /status should list the persisted row with the localized state
    // label and the linked resource detail href.
    let body = client
        .get(format!("http://{addr}/status"))
        .send()
        .await
        .expect("GET /status")
        .text()
        .await
        .expect("body text");
    assert!(
        body.contains("postgres-instance"),
        "/status should show the kind; body: {body}"
    );
    assert!(
        body.contains("PostgreSQL / primary"),
        "/status should show the component label + instance name"
    );
    assert!(
        body.contains("PostgreSQL 17.2"),
        "/status should show the server_version from the status JSON"
    );
    assert!(
        body.contains(r#"href="/resource/postgres-instance/primary""#),
        "/status row should link to the resource detail page"
    );
    assert!(
        body.contains("Observing"),
        "/status should show the localized Observing state for a healthy row"
    );

    // /resource/postgres-instance/primary should render the spec + status.
    let resp = client
        .get(format!("http://{addr}/resource/postgres-instance/primary"))
        .send()
        .await
        .expect("GET /resource/postgres-instance/primary");
    assert!(
        resp.status().is_success(),
        "resource detail status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("postgres-instance / primary"));
    assert!(body.contains("Desired spec"));
    // JSON is HTML-escaped before display, so quotes appear as &quot;.
    assert!(body.contains("&quot;port&quot;: 5432"));
    assert!(body.contains("Observed status"));
    assert!(body.contains("PostgreSQL 17.2"));
    assert!(
        body.contains("Revision"),
        "resource page should label the revision row"
    );

    // /resource for a kind/name that doesn't exist should be 404 with
    // the localized not-found page (not a raw 404 string).
    let resp = client
        .get(format!("http://{addr}/resource/postgres-instance/missing"))
        .send()
        .await
        .expect("GET /resource/postgres-instance/missing");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "missing resource should return HTTP 404"
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("not in the metadata store"));

    server.abort();
}
