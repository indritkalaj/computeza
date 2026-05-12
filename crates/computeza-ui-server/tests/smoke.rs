//! Server-level smoke test: bind to a free port, serve, hit /healthz and /,
//! and assert the responses contain localized strings.
//!
//! Runs by default -- no external dependencies. The OS picks a free port via
//! port 0 binding so concurrent test runs don't collide.

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;

// `serde_json` is brought in transitively by the ui-server crate.

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

    // /healthz -- localized "ok"
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

    // / -- full HTML page with localized welcome strings
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

    // /static/computeza.css -- embedded Tailwind-compatible utility CSS
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

    // /api/state/info -- with no store attached, reports store_attached=false
    let resp = client
        .get(format!("http://{addr}/api/state/info"))
        .send()
        .await
        .expect("GET /api/state/info");
    assert!(
        resp.status().is_success(),
        "/api/state/info status: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["store_attached"], false);

    // /install -- hub of component cards.
    let resp = client
        .get(format!("http://{addr}/install"))
        .send()
        .await
        .expect("GET /install");
    assert!(
        resp.status().is_success(),
        "/install status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Choose a component"));
    assert!(body.contains("PostgreSQL"));
    assert!(body.contains("Kanidm"));
    assert!(body.contains(r#"href="/install/postgres""#));

    // /install/postgres -- the actual install wizard form.
    let resp = client
        .get(format!("http://{addr}/install/postgres"))
        .send()
        .await
        .expect("GET /install/postgres");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains(r#"action="/install/postgres""#));

    // /install/kanidm -- kanidm has its own wizard now (Linux
    // `cargo install` flow). Available: true on the hub.
    let resp = client
        .get(format!("http://{addr}/install/kanidm"))
        .send()
        .await
        .expect("GET /install/kanidm");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Kanidm"));
    assert!(body.contains(r#"action="/install/kanidm""#));

    // /install/<other-still-planned> -- still the CLI explainer.
    let resp = client
        .get(format!("http://{addr}/install/garage"))
        .send()
        .await
        .expect("GET /install/garage");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install from the CLI"));

    // /status -- with no store attached, surfaces the store-missing hint
    let resp = client
        .get(format!("http://{addr}/status"))
        .send()
        .await
        .expect("GET /status");
    assert!(
        resp.status().is_success(),
        "/status status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Reconciler status"));
    assert!(body.contains("No metadata store is attached"));

    // /components -- every spec section 2.2 component should be listed
    let resp = client
        .get(format!("http://{addr}/components"))
        .send()
        .await
        .expect("GET /components");
    assert!(
        resp.status().is_success(),
        "/components status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    for c in [
        "Kanidm",
        "Garage",
        "Lakekeeper",
        "Databend",
        "Qdrant",
        "Restate",
        "GreptimeDB",
        "Grafana",
        "PostgreSQL",
        "OpenFGA",
    ] {
        assert!(body.contains(c), "/components should mention {c}");
    }

    // Tear down. We abort rather than initiate graceful shutdown -- sufficient
    // for a smoke test, and avoids needing a shutdown channel in the public API.
    server.abort();
}
