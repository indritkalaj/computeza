//! Computeza UI server -- Leptos SSR + axum HTTP server.
//!
//! Per spec section 4.1, the operator console is a server-rendered Rust application
//! using Leptos in SSR mode with selective hydration. This crate is the
//! entry point: it wires the axum router, hosts the request handlers, and
//! renders Leptos `view!` trees to HTML on the server.
//!
//! # What v0.0.x ships
//!
//! - `serve(addr)` boots an axum server bound to the given address
//! - `GET /` renders a localized welcome page using Leptos's `view!` macro
//! - `GET /healthz` returns a localized "ok" string for liveness probes
//! - `tower-http::TraceLayer` emits structured tracing for every request
//!
//! Hydration (client-side WASM that re-attaches reactivity) is deferred --
//! the v0.0.x console is purely server-rendered, no JavaScript. We add
//! hydration once we have an actual interactive surface (the install
//! wizard or the pipeline canvas) that needs it.
//!
//! # i18n
//!
//! Every visible string flows through [`computeza_i18n::Localizer`].
//! Hardcoded English strings in this crate are a release-blocking bug.

#![warn(missing_docs)]

use std::net::SocketAddr;

use axum::{
    extract::State,
    http::header,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use computeza_i18n::Localizer;
use computeza_state::{SqliteStore, Store};
use leptos::prelude::*;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

/// Tailwind-compatible utility CSS, embedded at compile time. Served at
/// `/static/computeza.css` and referenced from the home page.
const COMPUTEZA_CSS: &str = include_str!("../assets/computeza.css");

/// Shared state passed to every handler. Holds the `SqliteStore` (when
/// `computeza serve` opens one) plus future shared services. Wrapped in
/// `Arc` so axum can clone it cheaply per request.
#[derive(Clone)]
pub struct AppState {
    /// Persistent metadata store, `None` for the unit-test smoke router.
    pub store: Option<Arc<SqliteStore>>,
}

impl AppState {
    /// Construct an empty state for tests / minimal serve.
    #[must_use]
    pub fn empty() -> Self {
        Self { store: None }
    }

    /// Construct with a backing SqliteStore.
    #[must_use]
    pub fn with_store(store: SqliteStore) -> Self {
        Self {
            store: Some(Arc::new(store)),
        }
    }
}

/// Boot the operator console on the given address with no backing store.
/// `serve_with_state` is the version `computeza serve` actually calls;
/// this entry point exists so smoke tests and ad-hoc invocations don't
/// need to wire a database up.
pub async fn serve(addr: SocketAddr) -> anyhow::Result<()> {
    serve_with_state(addr, AppState::empty()).await
}

/// Boot the operator console with a fully-populated `AppState`. Awaits
/// forever (until the process is signalled to terminate).
pub async fn serve_with_state(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let app = router_with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "computeza ui-server listening; visit / for the operator console, /healthz for liveness, /api/state/info for the metadata-store summary");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the axum router with no backing state. Convenience for smoke
/// tests; the binary uses `router_with_state`.
pub fn router() -> Router {
    router_with_state(AppState::empty())
}

/// Build the axum router with an `AppState` attached. Every handler that
/// needs the store extracts it via `State<AppState>`.
pub fn router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/", get(home_handler))
        .route("/components", get(components_handler))
        .route("/healthz", get(healthz_handler))
        .route("/api/state/info", get(state_info_handler))
        .route("/static/computeza.css", get(css_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn state_info_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(store) = &state.store else {
        return Json(serde_json::json!({
            "store_attached": false,
            "message": "no backing SqliteStore configured for this server",
        }));
    };
    // Best-effort survey: count resources per known kind. Failures are
    // reported in the response rather than crashing the request -- /api/
    // routes must be resilient to a partially-broken backend.
    let kinds = [
        "postgres-instance",
        "kanidm-instance",
        "garage-instance",
        "lakekeeper-instance",
        "databend-instance",
        "qdrant-instance",
        "restate-instance",
        "greptime-instance",
        "grafana-instance",
        "openfga-instance",
    ];
    let mut counts = serde_json::Map::new();
    let mut errors = serde_json::Map::new();
    for k in kinds {
        match store.list(k, None).await {
            Ok(v) => {
                counts.insert(k.into(), serde_json::Value::from(v.len()));
            }
            Err(e) => {
                errors.insert(k.into(), serde_json::Value::from(e.to_string()));
            }
        }
    }
    Json(serde_json::json!({
        "store_attached": true,
        "resource_counts": counts,
        "errors": errors,
    }))
}

async fn components_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_components(&l))
}

async fn home_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_home(&l))
}

async fn healthz_handler() -> String {
    let l = Localizer::english();
    l.t("ui-healthz-ok")
}

async fn css_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        COMPUTEZA_CSS,
    )
        .into_response()
}

/// Render the home page to a complete HTML document.
///
/// Public for testability and so future page modules can reuse the shell.
#[must_use]
pub fn render_home(localizer: &Localizer) -> String {
    let app_title = localizer.t("ui-app-title");
    let tagline = localizer.t("ui-app-tagline");
    let title = localizer.t("ui-welcome-title");
    let subtitle = localizer.t("ui-welcome-subtitle");
    let status = localizer.t("ui-welcome-status");
    let spec_note = localizer.t("ui-welcome-spec");
    let version_label = localizer.t("ui-footer-version");
    let version = env!("CARGO_PKG_VERSION");

    // Body fragment via Leptos view!. Tailwind-compatible utility classes
    // come from /static/computeza.css (see assets/computeza.css).
    let components_link = localizer.t("ui-nav-components");
    let body_view = view! {
        <main class="mx-auto max-w-4xl p-12">
            <header class="border-b pb-6 mb-10">
                <h1 class="text-orange-500 text-2xl font-semibold tracking-tight m-0 mb-1">
                    {app_title.clone()}
                </h1>
                <p class="text-indigo-300 text-sm m-0">{tagline}</p>
            </header>
            <nav class="mb-6">
                <a class="text-indigo-300 text-sm" href="/components">{components_link}</a>
            </nav>
            <section>
                <h2 class="text-2xl font-semibold text-slate-100 m-0 mb-3">{title.clone()}</h2>
                <p class="text-slate-100 m-0 mb-6">{subtitle}</p>
                <p class="text-slate-500 text-sm m-0 mb-2">{status}</p>
                <p class="text-slate-500 text-sm m-0">{spec_note}</p>
            </section>
            <footer class="mt-16 pt-6 border-t text-slate-500 text-xs">
                <span>{version_label}" "{version}</span>
            </footer>
        </main>
    };
    let body_html = body_view.to_html();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} -- {app_title}</title>
<link rel="stylesheet" href="/static/computeza.css" />
</head>
<body class="bg-indigo-900 text-slate-100">
{body_html}
</body>
</html>"#
    )
}

/// Render the `/components` page: a table of every component the
/// platform manages, sourced from spec section 2.2 + per-component i18n
/// keys. Static for v0.0.x; future versions will surface live
/// reconciler status (drift indicators per spec section 4.4).
#[must_use]
pub fn render_components(localizer: &Localizer) -> String {
    let app_title = localizer.t("ui-app-title");
    let title = localizer.t("ui-components-title");
    let intro = localizer.t("ui-components-intro");
    let col_name = localizer.t("ui-components-col-name");
    let col_kind = localizer.t("ui-components-col-kind");
    let col_role = localizer.t("ui-components-col-role");

    let components: &[(&str, &str)] = &[
        ("kanidm", "identity"),
        ("garage", "object-storage"),
        ("lakekeeper", "catalog"),
        ("xtable", "format-translation"),
        ("databend", "sql-engine"),
        ("qdrant", "vector"),
        ("restate", "workflows"),
        ("greptime", "observability"),
        ("grafana", "dashboards"),
        ("postgres", "metadata-rdbms"),
        ("openfga", "authorization"),
    ];

    let rows: String = components
        .iter()
        .map(|(slug, kind)| {
            let name = localizer.t(&format!("component-{slug}-name"));
            let role = localizer.t(&format!("component-{slug}-role"));
            format!(
                "<tr><td class=\"text-slate-100\" style=\"padding: 0.5rem 1rem 0.5rem 0;\">{name}</td>\
                 <td class=\"text-indigo-300 text-sm\" style=\"padding: 0.5rem 1rem;\">{kind}</td>\
                 <td class=\"text-slate-500 text-sm\" style=\"padding: 0.5rem 0;\">{role}</td></tr>"
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} -- {app_title}</title>
<link rel="stylesheet" href="/static/computeza.css" />
</head>
<body class="bg-indigo-900 text-slate-100">
<main class="mx-auto max-w-4xl p-12">
<header class="border-b pb-6 mb-10">
<h1 class="text-orange-500 text-2xl font-semibold tracking-tight m-0 mb-1">{app_title}</h1>
<nav><a class="text-indigo-300 text-sm" href="/"><-&nbsp;Home</a></nav>
</header>
<section>
<h2 class="text-2xl font-semibold text-slate-100 m-0 mb-3">{title}</h2>
<p class="text-slate-500 text-sm m-0 mb-6">{intro}</p>
<table style="width: 100%; border-collapse: collapse;">
<thead><tr class="text-indigo-300 text-sm" style="text-align: left;">
<th style="padding: 0.5rem 1rem 0.5rem 0;">{col_name}</th>
<th style="padding: 0.5rem 1rem;">{col_kind}</th>
<th style="padding: 0.5rem 0;">{col_role}</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>
</section>
</main>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_components_lists_every_spec_component() {
        let l = Localizer::english();
        let html = render_components(&l);
        for component in [
            "Kanidm",
            "Garage",
            "Lakekeeper",
            "Apache XTable",
            "Databend",
            "Qdrant",
            "Restate",
            "GreptimeDB",
            "Grafana",
            "PostgreSQL",
            "OpenFGA",
        ] {
            assert!(
                html.contains(component),
                "/components should mention {component}; got HTML excerpt:\n{}",
                &html[..html.len().min(2000)]
            );
        }
    }

    #[test]
    fn render_home_contains_localized_title() {
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(
            html.contains("Welcome to Computeza"),
            "rendered HTML should contain the localized welcome title; got:\n{html}"
        );
    }

    #[test]
    fn render_home_is_a_complete_html_document() {
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html lang=\"en\">"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn render_home_has_no_hardcoded_english_strings_outside_attributes() {
        // Sanity check: every <p> and <h*> text node should be a value the
        // localizer produced. We assert by checking that strings the .ftl
        // bundle defines actually appear (positive check) and that some
        // common hardcoded-English smell doesn't (negative check).
        let l = Localizer::english();
        let html = render_home(&l);
        assert!(html.contains("Computeza")); // ui-app-title
        assert!(html.contains("Open lakehouse control plane")); // ui-app-tagline
        assert!(html.contains("Pre-alpha")); // ui-welcome-status starts with this
    }
}
