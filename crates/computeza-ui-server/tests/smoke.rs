//! Server-level smoke test: bind to a free port, serve, hit /healthz and /,
//! and assert the responses contain localized strings.
//!
//! Runs by default — no external dependencies. The OS picks a free port via
//! port 0 binding so concurrent test runs don't collide.

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;

#[tokio::test]
async fn server_serves_localized_home_and_healthz() {
    // Bind to an OS-chosen free port.
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind to free local port");
    let addr = listener.local_addr().expect("read bound addr");

    // Spawn the server using the public router (avoids needing a separate
    // graceful-shutdown story for the test).
    let server = tokio::spawn(async move {
        axum::serve(listener, computeza_ui_server::router())
            .await
            .expect("axum::serve")
    });

    // Build a client and hit the endpoints.
    let client = reqwest::Client::builder().build().expect("reqwest client");

    // /healthz — localized "ok"
    let resp = client
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert!(
        resp.status().is_success(),
        "/healthz status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert_eq!(
        body, "ok",
        "/healthz should return the localized ui-healthz-ok value"
    );

    // / — full HTML page with localized welcome strings
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("GET /");
    assert!(resp.status().is_success(), "/ status: {}", resp.status());
    let body = resp.text().await.expect("body text");
    assert!(
        body.starts_with("<!DOCTYPE html>"),
        "expected HTML document"
    );
    assert!(
        body.contains("Welcome to Computeza"),
        "rendered home should contain the localized welcome title"
    );
    assert!(
        body.contains("Open lakehouse control plane"),
        "rendered home should contain the localized tagline"
    );
    assert!(
        body.contains(r#"href="/static/computeza.css""#),
        "home should link the embedded stylesheet"
    );

    // /static/computeza.css — embedded Tailwind-compatible utility CSS
    let resp = client
        .get(format!("http://{addr}/static/computeza.css"))
        .send()
        .await
        .expect("GET /static/computeza.css");
    assert!(
        resp.status().is_success(),
        "/static/computeza.css status: {}",
        resp.status()
    );
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/css; charset=utf-8"),
        "CSS asset should be served with text/css content-type"
    );
    let css = resp.text().await.expect("body text");
    assert!(
        css.contains(".bg-indigo-900"),
        "embedded CSS should define spec section 4.3 palette utilities"
    );

    // Tear down. We abort rather than initiate graceful shutdown — sufficient
    // for a smoke test, and avoids needing a shutdown channel in the public API.
    server.abort();
}
