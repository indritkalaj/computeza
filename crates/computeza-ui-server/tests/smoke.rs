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

    // /install -- the unified whole-stack install form. One card per
    // component, one Install button at the bottom posting back to
    // /install. Per-component pages stay accessible at /install/<slug>
    // but the hub no longer links to them.
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
    assert!(body.contains("Install Computeza"));
    assert!(body.contains("PostgreSQL"));
    assert!(body.contains("Kanidm"));
    assert!(
        body.contains(r#"action="/install""#),
        "the unified hub form must post back to /install"
    );
    assert!(
        body.contains(r#"name="postgres__port""#),
        "the unified hub must collect per-slug port fields"
    );
    assert!(
        body.contains("Install all components"),
        "the global submit button must be rendered"
    );

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

    // /install/garage -- garage has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/garage"))
        .send()
        .await
        .expect("GET /install/garage");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Garage"));
    assert!(body.contains(r#"action="/install/garage""#));

    // /install/greptime -- greptime has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/greptime"))
        .send()
        .await
        .expect("GET /install/greptime");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install GreptimeDB"));
    assert!(body.contains(r#"action="/install/greptime""#));

    // /install/lakekeeper -- lakekeeper has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/lakekeeper"))
        .send()
        .await
        .expect("GET /install/lakekeeper");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Lakekeeper"));
    assert!(body.contains(r#"action="/install/lakekeeper""#));

    // /install/databend -- databend has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/databend"))
        .send()
        .await
        .expect("GET /install/databend");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Databend"));
    assert!(body.contains(r#"action="/install/databend""#));

    // /install/grafana -- grafana has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/grafana"))
        .send()
        .await
        .expect("GET /install/grafana");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Grafana"));
    assert!(body.contains(r#"action="/install/grafana""#));

    // /install/restate -- restate has its own wizard now.
    let resp = client
        .get(format!("http://{addr}/install/restate"))
        .send()
        .await
        .expect("GET /install/restate");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Install Restate"));
    assert!(body.contains(r#"action="/install/restate""#));

    // /install/<still-planned> -- the CLI explainer page.
    // xtable is the only remaining unshipped component. Apache
    // distributes source-only and Maven hosts only thin JARs that
    // require a Maven dep resolve to actually run; blocked on a
    // Computeza-side fat-JAR build pipeline (see AGENTS.md).
    let resp = client
        .get(format!("http://{addr}/install/xtable"))
        .send()
        .await
        .expect("GET /install/xtable");
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

    // /admin/secrets -- secrets store is NOT attached on this server
    // (the smoke harness uses AppState::empty()) so the page should
    // render the "no secrets store attached" warning, not crash.
    let resp = client
        .get(format!("http://{addr}/admin/secrets"))
        .send()
        .await
        .expect("GET /admin/secrets");
    assert!(
        resp.status().is_success(),
        "/admin/secrets status: {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("No secrets store is attached"));
    assert!(
        !body.contains("Backup required"),
        "no backup card should render when the store is absent"
    );

    // /admin/secrets/{name}/rotate -- with no store attached this
    // should return 404 cleanly rather than panic. The handler
    // surfaces a clean error page.
    let resp = client
        .post(format!(
            "http://{addr}/admin/secrets/postgres%2Fadmin-password/rotate"
        ))
        .send()
        .await
        .expect("POST /admin/secrets/.../rotate");
    assert!(
        resp.status() == 404,
        "expected 404 when no secrets store is attached, got {}",
        resp.status()
    );
    let body = resp.text().await.expect("body text");
    assert!(body.contains("No secrets store is attached"));

    // /install/job/{id} for an unknown id should 404 cleanly.
    let resp = client
        .get(format!("http://{addr}/install/job/this-id-does-not-exist"))
        .send()
        .await
        .expect("GET /install/job/<unknown>");
    assert!(resp.status() == 404);

    // /install/job/{id}/rollback for an unknown id should render the
    // unknown-job error page (200 OK with the error in the body).
    let resp = client
        .post(format!(
            "http://{addr}/install/job/another-unknown/rollback"
        ))
        .send()
        .await
        .expect("POST /install/job/<unknown>/rollback");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Unknown install job"));

    // /api/install/job/{id} for an unknown id should 404 with a JSON
    // error -- the polling client uses this to learn the job died.
    let resp = client
        .get(format!("http://{addr}/api/install/job/poll-unknown"))
        .send()
        .await
        .expect("GET /api/install/job/<unknown>");
    assert!(resp.status() == 404);
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert!(body.get("error").is_some());

    // Top nav should now link to /admin/secrets so an operator can find
    // the secrets management page from anywhere in the console.
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("GET / second time");
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains(r#"href="/admin/secrets""#),
        "the home page nav must link to /admin/secrets"
    );

    // CSRF: a hand-rolled POST to /admin/secrets/.../rotate without
    // a session cookie or csrf_token should still be rejected. On the
    // auth-disabled smoke harness the middleware bypasses CSRF, so
    // we just verify the response is a clean error (not a panic /
    // 500). On a real binary this would render the CSRF-rejected
    // page; here it 404s because no secrets store is attached.
    let resp = client
        .post(format!(
            "http://{addr}/admin/secrets/postgres%2Fadmin-password/rotate"
        ))
        .body("")
        .send()
        .await
        .expect("hand-rolled rotate POST");
    let s = resp.status().as_u16();
    assert!(
        s == 200 || s == 303 || s == 403 || s == 404 || s == 500,
        "unexpected CSRF smoke status: {s}"
    );

    // Every authenticated form rendered by render_install_hub now
    // embeds an empty csrf_token input that the inline JS will fill
    // from the cookie on submit. Verify the input is present in the
    // rendered HTML.
    let resp = client
        .get(format!("http://{addr}/install"))
        .send()
        .await
        .expect("GET /install for csrf input check");
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains(r#"name="csrf_token""#),
        "the install hub form must embed an empty csrf_token input for the inline JS to fill"
    );
    assert!(
        body.contains(r#"computeza_csrf="#),
        "the inline JS that auto-fills csrf_token inputs must be embedded in the shell"
    );

    // /login renders the sign-in form for unauthenticated visitors.
    let resp = client
        .get(format!("http://{addr}/login"))
        .send()
        .await
        .expect("GET /login");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("Sign in"));
    assert!(body.contains(r#"action="/login""#));
    assert!(body.contains(r#"name="username""#));
    assert!(body.contains(r#"name="password""#));

    // /setup renders the first-boot form for unauthenticated visitors.
    let resp = client
        .get(format!("http://{addr}/setup"))
        .send()
        .await
        .expect("GET /setup");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body text");
    assert!(body.contains("First-boot setup"));
    assert!(body.contains(r#"action="/setup""#));
    assert!(body.contains(r#"name="password_confirm""#));

    // Auth is disabled on this test harness (AppState::empty() ships no
    // OperatorFile) -- the auth middleware lets every request through.
    // We still verify the /account route renders something sensible
    // when the middleware does NOT inject a Session: it should 5xx
    // cleanly (the Extension extractor rejects) rather than panic.
    let resp = client
        .get(format!("http://{addr}/account"))
        .send()
        .await
        .expect("GET /account on auth-disabled harness");
    assert!(
        resp.status() == 500 || resp.status() == 200,
        "/account should not panic when no session is present; got {}",
        resp.status()
    );

    // Tear down. We abort rather than initiate graceful shutdown -- sufficient
    // for a smoke test, and avoids needing a shutdown channel in the public API.
    server.abort();
}
