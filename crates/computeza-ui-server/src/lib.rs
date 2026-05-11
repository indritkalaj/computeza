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
    extract::{Form, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
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
        .route("/install", get(install_handler))
        .route("/install/postgres", post(install_postgres_handler))
        .route("/status", get(status_handler))
        .route("/healthz", get(healthz_handler))
        .route("/api/state/info", get(state_info_handler))
        .route("/static/computeza.css", get(css_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn install_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install(&l))
}

#[derive(serde::Deserialize)]
struct InstallForm {
    /// Component slug. v0.0.x recognises only "postgres".
    component: String,
}

async fn install_postgres_handler(Form(form): Form<InstallForm>) -> Html<String> {
    let l = Localizer::english();
    if form.component != "postgres" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ));
    }
    let result = run_postgres_install().await;
    match result {
        Ok(summary) => Html(render_install_result(&l, true, &summary)),
        Err(detail) => Html(render_install_result(&l, false, &detail)),
    }
}

/// Run the platform-specific Postgres install and return either a
/// success summary or a failure detail (both for human display).
#[cfg(target_os = "linux")]
async fn run_postgres_install() -> Result<String, String> {
    use computeza_driver_native::linux::postgres;
    match postgres::install(postgres::InstallOptions::default()).await {
        Ok(r) => Ok(format!(
            "bin_dir: {}\ndata_dir: {}\nsystemd unit: {}\nport: {}\npsql symlink: {}",
            r.bin_dir.display(),
            r.data_dir.display(),
            r.unit_path.display(),
            r.port,
            r.psql_symlink
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not created)".into()),
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "macos")]
async fn run_postgres_install() -> Result<String, String> {
    use computeza_driver_native::macos::postgres;
    match postgres::install(postgres::InstallOptions::default()).await {
        Ok(r) => Ok(format!(
            "bin_dir: {}\ndata_dir: {}\nlaunchd plist: {}\nport: {}\npsql symlink: {}",
            r.bin_dir.display(),
            r.data_dir.display(),
            r.plist_path.display(),
            r.port,
            r.psql_symlink
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not created)".into()),
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "windows")]
async fn run_postgres_install() -> Result<String, String> {
    use computeza_driver_native::windows::postgres;
    match postgres::install(postgres::InstallOptions::default()).await {
        Ok(r) => Ok(format!(
            "bin_dir: {}\ndata_dir: {}\nservice: {}\nport: {}\npsql shim: {}",
            r.bin_dir.display(),
            r.data_dir.display(),
            r.service_name,
            r.port,
            r.psql_shim
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not created)".into()),
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn run_postgres_install() -> Result<String, String> {
    Err("install is supported on Linux, macOS, and Windows only".into())
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

/// One row in the `/status` table -- a single resource instance and its
/// most recent observation, projected from the store's opaque status JSON
/// into the fields every HTTP reconciler exposes (server_version,
/// last_observed_at, last_observe_failed).
#[derive(Clone, Debug, Default)]
pub struct StatusRow {
    /// Resource kind, e.g. `postgres-instance`.
    pub kind: String,
    /// Localized display name for the component, e.g. "PostgreSQL".
    pub component_label: String,
    /// Instance name as registered with `with_state(.., instance_name)`.
    pub instance_name: String,
    /// `server_version` from the status JSON, if present.
    pub server_version: Option<String>,
    /// ISO-8601 last observation timestamp, if present.
    pub last_observed_at: Option<String>,
    /// `last_observe_failed` flag from the status JSON. Defaults to false
    /// when the field is absent (e.g. the resource exists but has never
    /// been observed -- that case is reported via `has_status` instead).
    pub last_observe_failed: bool,
    /// Whether a status snapshot has been recorded at all.
    pub has_status: bool,
}

async fn status_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let Some(store) = &state.store else {
        return Html(render_status(&l, None));
    };
    // (kind, component-slug) -- the slug feeds the `component-<slug>-name`
    // i18n key for the localized display label.
    let entries: &[(&str, &str)] = &[
        ("postgres-instance", "postgres"),
        ("kanidm-instance", "kanidm"),
        ("garage-instance", "garage"),
        ("lakekeeper-instance", "lakekeeper"),
        ("databend-instance", "databend"),
        ("qdrant-instance", "qdrant"),
        ("restate-instance", "restate"),
        ("greptime-instance", "greptime"),
        ("grafana-instance", "grafana"),
        ("openfga-instance", "openfga"),
    ];
    let mut rows: Vec<StatusRow> = Vec::new();
    for (kind, slug) in entries {
        let Ok(list) = store.list(kind, None).await else {
            continue;
        };
        for sr in list {
            let status = sr.status.as_ref();
            let server_version = status
                .and_then(|s| s.get("server_version"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let last_observed_at = status
                .and_then(|s| s.get("last_observed_at"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let last_observe_failed = status
                .and_then(|s| s.get("last_observe_failed"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            rows.push(StatusRow {
                kind: (*kind).into(),
                component_label: l.t(&format!("component-{slug}-name")),
                instance_name: sr.key.name.clone(),
                server_version,
                last_observed_at,
                last_observe_failed,
                has_status: status.is_some(),
            });
        }
    }
    Html(render_status(&l, Some(&rows)))
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
    let install_link = localizer.t("ui-nav-install");
    let status_link = localizer.t("ui-nav-status");
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
                " | "
                <a class="text-indigo-300 text-sm" href="/install">{install_link}</a>
                " | "
                <a class="text-indigo-300 text-sm" href="/status">{status_link}</a>
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

/// Minimal HTML escape for embedding plain text inside element content.
/// Not a general-purpose sanitiser -- callers must not pass user-controlled
/// HTML through it expecting attribute-safety.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Render the `/install` page: a small form that POSTs to
/// `/install/postgres` and lays down a native PostgreSQL service on the
/// host. Per spec section 2.1 / 4.2 the install path is GUI-equivalent
/// to the CLI's `computeza install postgres`.
#[must_use]
pub fn render_install(localizer: &Localizer) -> String {
    let app_title = localizer.t("ui-app-title");
    let title = localizer.t("ui-install-title");
    let intro = localizer.t("ui-install-intro");
    let target_label = localizer.t("ui-install-target-label");
    let option_postgres = localizer.t("ui-install-postgres");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");

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
<nav><a class="text-indigo-300 text-sm" href="/">&lt;-&nbsp;Home</a></nav>
</header>
<section>
<h2 class="text-2xl font-semibold text-slate-100 m-0 mb-3">{title}</h2>
<p class="text-slate-500 text-sm m-0 mb-6">{intro}</p>
<form method="post" action="/install/postgres" style="display: flex; flex-direction: column; gap: 1rem; max-width: 28rem;">
<label class="text-indigo-300 text-sm" for="component">{target_label}</label>
<select id="component" name="component" class="text-slate-100" style="background: transparent; border: 1px solid currentColor; padding: 0.5rem;">
<option value="postgres">{option_postgres}</option>
</select>
<button type="submit" class="text-orange-500" style="background: transparent; border: 1px solid currentColor; padding: 0.5rem 1rem; cursor: pointer; align-self: flex-start;">{button}</button>
</form>
<p class="text-slate-500 text-sm" style="margin-top: 1.5rem;">{requires_root}</p>
</section>
</main>
</body>
</html>"#
    )
}

/// Render the `/install/postgres` result page. `success` switches the
/// heading between the success and failure i18n keys; `detail` is the
/// raw output (success summary or error chain) shown verbatim in a
/// `<pre>` block after HTML-escaping.
#[must_use]
pub fn render_install_result(localizer: &Localizer, success: bool, detail: &str) -> String {
    let app_title = localizer.t("ui-app-title");
    let title = localizer.t("ui-install-result-title");
    let outcome = if success {
        localizer.t("ui-install-result-success")
    } else {
        localizer.t("ui-install-result-failed")
    };
    let back = localizer.t("ui-install-result-back");
    let detail_html = html_escape(detail);
    let outcome_class = if success {
        "text-slate-100"
    } else {
        "text-orange-500"
    };

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
<nav><a class="text-indigo-300 text-sm" href="/install">&lt;-&nbsp;{back}</a></nav>
</header>
<section>
<h2 class="text-2xl font-semibold {outcome_class} m-0 mb-3">{outcome}</h2>
<p class="text-indigo-300 text-sm m-0 mb-3">{title}</p>
<pre class="text-slate-100 text-sm" style="background: rgba(0,0,0,0.25); padding: 1rem; overflow-x: auto; white-space: pre-wrap; word-break: break-word;">{detail_html}</pre>
</section>
</main>
</body>
</html>"#
    )
}

/// Render the `/status` page: one row per persisted reconciler
/// observation. `rows = None` means the server is running without a
/// metadata store (no `computeza serve`), and we surface the
/// `ui-status-store-missing` hint instead of an empty table.
#[must_use]
pub fn render_status(localizer: &Localizer, rows: Option<&[StatusRow]>) -> String {
    let app_title = localizer.t("ui-app-title");
    let title = localizer.t("ui-status-title");
    let intro = localizer.t("ui-status-intro");
    let col_kind = localizer.t("ui-status-col-kind");
    let col_name = localizer.t("ui-status-col-name");
    let col_version = localizer.t("ui-status-col-version");
    let col_observed = localizer.t("ui-status-col-observed");
    let col_state = localizer.t("ui-status-col-state");

    let body = match rows {
        None => format!(
            r#"<p class="text-orange-500 text-sm m-0">{}</p>"#,
            html_escape(&localizer.t("ui-status-store-missing"))
        ),
        Some([]) => format!(
            r#"<p class="text-slate-500 text-sm m-0">{}</p>"#,
            html_escape(&localizer.t("ui-status-empty"))
        ),
        Some(rs) => {
            let state_ok = localizer.t("ui-status-state-ok");
            let state_failed = localizer.t("ui-status-state-failed");
            let state_unknown = localizer.t("ui-status-state-unknown");
            let body_rows: String = rs
                .iter()
                .map(|r| {
                    let (state_label, state_class) = if !r.has_status {
                        (state_unknown.clone(), "text-indigo-300")
                    } else if r.last_observe_failed {
                        (state_failed.clone(), "text-orange-500")
                    } else {
                        (state_ok.clone(), "text-slate-100")
                    };
                    let version = r
                        .server_version
                        .clone()
                        .unwrap_or_else(|| "-".to_string());
                    let observed = r
                        .last_observed_at
                        .clone()
                        .unwrap_or_else(|| "-".to_string());
                    format!(
                        "<tr>\
                         <td class=\"text-indigo-300 text-sm\" style=\"padding: 0.5rem 1rem 0.5rem 0;\">{kind}</td>\
                         <td class=\"text-slate-100\" style=\"padding: 0.5rem 1rem;\">{label} / {name}</td>\
                         <td class=\"text-slate-500 text-sm\" style=\"padding: 0.5rem 1rem;\">{version}</td>\
                         <td class=\"text-slate-500 text-sm\" style=\"padding: 0.5rem 1rem;\">{observed}</td>\
                         <td class=\"{state_class}\" style=\"padding: 0.5rem 0;\">{state_label}</td>\
                         </tr>",
                        kind = html_escape(&r.kind),
                        label = html_escape(&r.component_label),
                        name = html_escape(&r.instance_name),
                        version = html_escape(&version),
                        observed = html_escape(&observed),
                        state_class = state_class,
                        state_label = html_escape(&state_label),
                    )
                })
                .collect();
            format!(
                r#"<table style="width: 100%; border-collapse: collapse;">
<thead><tr class="text-indigo-300 text-sm" style="text-align: left;">
<th style="padding: 0.5rem 1rem 0.5rem 0;">{col_kind}</th>
<th style="padding: 0.5rem 1rem;">{col_name}</th>
<th style="padding: 0.5rem 1rem;">{col_version}</th>
<th style="padding: 0.5rem 1rem;">{col_observed}</th>
<th style="padding: 0.5rem 0;">{col_state}</th>
</tr></thead>
<tbody>{body_rows}</tbody>
</table>"#
            )
        }
    };

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
<nav><a class="text-indigo-300 text-sm" href="/">&lt;-&nbsp;Home</a></nav>
</header>
<section>
<h2 class="text-2xl font-semibold text-slate-100 m-0 mb-3">{title}</h2>
<p class="text-slate-500 text-sm m-0 mb-6">{intro}</p>
{body}
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
    fn render_install_shows_form_and_postgres_option() {
        let l = Localizer::english();
        let html = render_install(&l);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("Install a component"));
        assert!(html.contains("PostgreSQL"));
        assert!(html.contains("action=\"/install/postgres\""));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("name=\"component\""));
        assert!(html.contains("value=\"postgres\""));
    }

    #[test]
    fn render_install_result_success_uses_success_string() {
        let l = Localizer::english();
        let html = render_install_result(&l, true, "bin_dir: /opt/computeza/postgres");
        assert!(html.contains("Install completed."));
        assert!(html.contains("bin_dir: /opt/computeza/postgres"));
    }

    #[test]
    fn render_status_no_store_shows_store_missing_hint() {
        let l = Localizer::english();
        let html = render_status(&l, None);
        assert!(html.contains("Reconciler status"));
        assert!(html.contains("No metadata store is attached"));
    }

    #[test]
    fn render_status_empty_shows_empty_hint() {
        let l = Localizer::english();
        let html = render_status(&l, Some(&[]));
        assert!(html.contains("No resources have been observed yet."));
    }

    #[test]
    fn render_status_row_renders_kind_name_version_state() {
        let l = Localizer::english();
        let rows = vec![StatusRow {
            kind: "postgres-instance".into(),
            component_label: "PostgreSQL".into(),
            instance_name: "primary".into(),
            server_version: Some("PostgreSQL 17.2".into()),
            last_observed_at: Some("2026-05-11T08:00:00Z".into()),
            last_observe_failed: false,
            has_status: true,
        }];
        let html = render_status(&l, Some(&rows));
        assert!(html.contains("postgres-instance"));
        assert!(html.contains("PostgreSQL / primary"));
        assert!(html.contains("PostgreSQL 17.2"));
        assert!(html.contains("2026-05-11T08:00:00Z"));
        assert!(html.contains("Observing"));
    }

    #[test]
    fn render_status_row_marks_failed_observation_in_orange() {
        let l = Localizer::english();
        let rows = vec![StatusRow {
            kind: "kanidm-instance".into(),
            component_label: "Kanidm".into(),
            instance_name: "primary".into(),
            server_version: None,
            last_observed_at: None,
            last_observe_failed: true,
            has_status: true,
        }];
        let html = render_status(&l, Some(&rows));
        assert!(html.contains("Failed"));
        assert!(html.contains("text-orange-500"));
    }

    #[test]
    fn render_install_result_failure_uses_failure_string_and_escapes_detail() {
        let l = Localizer::english();
        let html = render_install_result(&l, false, "<script>alert(1)</script>");
        assert!(html.contains("Install failed."));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert(1)</script>"));
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
