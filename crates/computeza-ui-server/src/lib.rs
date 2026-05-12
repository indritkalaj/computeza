//! Computeza UI server -- axum HTTP server, server-rendered HTML.
//!
//! Per spec section 4.1, the operator console is a server-rendered
//! Rust application. v0.0.x renders HTML directly from string
//! templates; Leptos `view!` trees will return for v0.1 once we have
//! interactive surfaces (the pipeline canvas) that benefit from
//! reactive primitives.
//!
//! # What v0.0.x ships
//!
//! - `serve(addr)` boots an axum server bound to the given address.
//! - The pages `/`, `/components`, `/install`, `/install/job/{id}`,
//!   `/status`, `/resource/{kind}/{name}` make up the operator surface.
//! - `/api/install/job/{id}` and `/api/state/info` are the JSON
//!   endpoints behind the wizard and the metadata-store card.
//! - `tower-http::TraceLayer` emits structured tracing per request.
//!
//! All UI pages share the `render_shell` helper, which lays down the
//! top navigation, page container, and footer. Individual page
//! renderers produce the body fragment.
//!
//! # i18n
//!
//! Every visible string flows through [`computeza_i18n::Localizer`].
//! Hardcoded English strings in this crate are a release-blocking bug.

#![warn(missing_docs)]

pub mod auth;

use std::net::SocketAddr;

use axum::{
    extract::{Form, Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use computeza_driver_native::progress::{InstallProgress, ProgressHandle};
use computeza_i18n::Localizer;
use computeza_secrets::SecretsStore;
use computeza_state::{ResourceKey, SqliteStore, Store};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

/// Hand-maintained design-system stylesheet, embedded at compile time.
/// Served at `/static/computeza.css` and referenced from every page.
const COMPUTEZA_CSS: &str = include_str!("../assets/computeza.css");

/// Brand assets, embedded at compile time so the binary has no
/// runtime dependency on a filesystem layout. Sourced from the brand
/// pack at `assets/brand/`. The favicon SVG is the small chip-die
/// mark with the lavender->pink gradient; the full logo is the same
/// mark refined for navigation use.
const COMPUTEZA_LOGO_SVG: &str = include_str!("../assets/brand/logo/computeza-logo.svg");
const COMPUTEZA_LOGO_MONO_LIGHT_SVG: &str =
    include_str!("../assets/brand/logo/computeza-logo-mono-light.svg");
const COMPUTEZA_FAVICON_SVG: &str = include_str!("../assets/brand/logo/computeza-favicon.svg");
const COMPUTEZA_LOTTIE_PULSE: &str = include_str!("../assets/brand/lottie/computeza-pulse.json");

/// In-process registry of background install jobs. Each entry maps a
/// freshly-minted UUID to the shared progress state the driver writes
/// into and the wizard polls. Jobs live for the lifetime of the
/// process -- v0.0.x doesn't persist them; restarting the server
/// forgets in-flight work.
pub type JobRegistry = Arc<StdMutex<HashMap<String, Arc<StdMutex<InstallProgress>>>>>;

/// Shared state passed to every handler. Holds the `SqliteStore` (when
/// `computeza serve` opens one), the encrypted [`SecretsStore`] (when
/// the operator has set `COMPUTEZA_SECRETS_PASSPHRASE`), the operator
/// account store + session table for auth, and the background-job
/// registry. Wrapped in `Arc` so axum can clone it cheaply per
/// request.
#[derive(Clone)]
pub struct AppState {
    /// Persistent metadata store, `None` for the unit-test smoke router.
    pub store: Option<Arc<SqliteStore>>,
    /// Background install jobs in flight or recently finished.
    pub jobs: JobRegistry,
    /// Encrypted secrets store. `None` when the operator hasn't set
    /// `COMPUTEZA_SECRETS_PASSPHRASE`; install paths that generate
    /// credentials degrade to surfacing them in-band on the result
    /// page instead of persisting them encrypted.
    pub secrets: Option<Arc<SecretsStore>>,
    /// Operator account store. `None` for the smoke-test harness; the
    /// real binary always attaches one (auth is disabled wholesale
    /// when this is None, intended only for unit-test surfaces).
    pub operators: Option<auth::OperatorFile>,
    /// In-process session table.
    pub sessions: auth::SessionStore,
    /// Append-only audit log (ed25519-signed, chained). `None` for
    /// the smoke-test harness; the binary always attaches one.
    /// Login / logout / setup flows route through this when present.
    pub audit: Option<Arc<computeza_audit::AuditLog>>,
}

impl AppState {
    /// Construct an empty state for tests / minimal serve.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            store: None,
            jobs: Arc::new(StdMutex::new(HashMap::new())),
            secrets: None,
            operators: None,
            sessions: auth::SessionStore::new(),
            audit: None,
        }
    }

    /// Construct with a backing SqliteStore.
    #[must_use]
    pub fn with_store(store: SqliteStore) -> Self {
        Self {
            store: Some(Arc::new(store)),
            jobs: Arc::new(StdMutex::new(HashMap::new())),
            secrets: None,
            operators: None,
            sessions: auth::SessionStore::new(),
            audit: None,
        }
    }

    /// Attach an audit log. Chainable with the other builders.
    #[must_use]
    pub fn with_audit(mut self, audit: computeza_audit::AuditLog) -> Self {
        self.audit = Some(Arc::new(audit));
        self
    }

    /// Attach an encrypted [`SecretsStore`] to the state. Chainable
    /// with [`AppState::with_store`].
    #[must_use]
    pub fn with_secrets(mut self, secrets: SecretsStore) -> Self {
        self.secrets = Some(Arc::new(secrets));
        self
    }

    /// Attach the operator account store. Once attached, the auth
    /// middleware enforces login on every non-public route.
    #[must_use]
    pub fn with_operators(mut self, operators: auth::OperatorFile) -> Self {
        self.operators = Some(operators);
        self
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

/// Auth middleware -- gates every non-public path behind a valid
/// session cookie.
///
/// Public paths (landing page, /login, /setup, /static/*, /healthz,
/// /favicon.ico, /components) flow through unchanged. Everything
/// else looks up the `computeza_session` cookie against
/// [`auth::SessionStore`]; a missing or stale cookie redirects to
/// `/login` with `?next=<original-path>` so the operator lands back
/// on the page they wanted after signing in.
///
/// When `state.operators` is `None` (the unit-test smoke router)
/// auth is disabled wholesale -- the middleware lets every request
/// through. The real binary always attaches an [`auth::OperatorFile`].
async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();

    if auth::is_public_path(path) {
        return next.run(request).await;
    }
    if state.operators.is_none() {
        // No operator file attached -- smoke-test surface. Skip auth.
        return next.run(request).await;
    }

    let cookie_header = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let session_id = cookie_header
        .as_deref()
        .and_then(auth::session_id_from_cookies);
    let session = match session_id {
        Some(id) => state.sessions.get(&id).await,
        None => None,
    };

    let Some(session) = session else {
        // Redirect to /login with the original path captured so we
        // can land them back here after sign-in.
        let next_url = format!(
            "/login?next={}",
            urlencoding_min(&format!(
                "{}{}",
                path,
                request
                    .uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default()
            ))
        );
        return Redirect303(next_url).into_response();
    };

    let mut request = request;
    request.extensions_mut().insert(session);
    next.run(request).await
}

/// Permission middleware -- runs after auth_middleware. Reads the
/// session's bound username, looks up the operator's groups, computes
/// the effective permission set, and verifies the route's required
/// permission is in that set.
///
/// Bypasses public paths and unauthenticated test surfaces (no
/// operators attached). Rejects with 403 + a clean inline page on
/// permission denial.
async fn permission_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().to_string();

    if auth::is_public_path(&path) {
        return next.run(request).await;
    }
    let Some(operators) = state.operators.clone() else {
        // No operator store attached = smoke-test surface = no auth.
        return next.run(request).await;
    };
    // The auth_middleware that ran before us already injected a
    // Session; if it's missing here we treat it as missing auth and
    // bounce back to /login (defensive -- this branch should not
    // hit under normal middleware ordering).
    let Some(session) = request.extensions().get::<auth::Session>().cloned() else {
        return Redirect303("/login".into()).into_response();
    };

    let Some(operator) = operators.get(&session.username).await else {
        // Session points at a non-existent operator (e.g. account
        // deleted while their cookie was still live). Clear the
        // cookie and bounce to /login so they can re-sign-in as
        // someone else.
        tracing::warn!(
            username = %session.username,
            "session references a deleted operator account; rejecting"
        );
        let mut response = Redirect303("/login".into()).into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&auth::clear_session_cookie_header()) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        if let Ok(value) = axum::http::HeaderValue::from_str(&auth::clear_csrf_cookie_header()) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        return response;
    };

    let perms = auth::permissions_for_groups(&operator.groups);
    let Some(required) = auth::required_permission_for(&method, &path) else {
        return next.run(request).await;
    };
    if !perms.contains(&required) {
        tracing::warn!(
            username = %session.username,
            method = %method,
            path = %path,
            required = ?required,
            groups = ?operator.groups,
            "permission denied"
        );
        return (
            StatusCode::FORBIDDEN,
            Html(render_permission_denied(&operator.groups, required)),
        )
            .into_response();
    }

    next.run(request).await
}

/// Tiny inline page rendered on permission denial. Not localized --
/// operators see this when they hit a button they don't have rights
/// for, which is rare in practice and the message just needs to be
/// understandable enough to ask their admin.
fn render_permission_denied(groups: &[String], required: auth::Permission) -> String {
    format!(
        "<!DOCTYPE html><html><head><title>Permission denied</title></head>\
         <body style=\"font-family:sans-serif;padding:2rem;max-width:40rem;margin:0 auto;\">\
         <h1>Permission denied</h1>\
         <p>Your account is in groups {groups:?} and does not carry the \
         <code>{required:?}</code> permission required for this surface. \
         Ask an administrator to add you to a group that includes it \
         (admins / operators / viewers).</p>\
         <p><a href=\"/\">Back to the landing page</a> &middot; \
         <a href=\"/account\">Your account</a></p>\
         </body></html>"
    )
}

/// CSRF middleware -- verifies the `csrf_token` form field on every
/// POST request to a non-public, non-exempt path.
///
/// `SameSite=Strict` on the session cookie already blocks cross-site
/// POSTs at the browser level for v0.0.x's loopback-bound trust model;
/// this middleware is the defense-in-depth layer that catches the rest
/// (subdomain attacks if the console is later exposed, intermediary
/// proxies that strip SameSite, etc.).
///
/// The middleware:
///   1. Bypasses GET / HEAD requests (CSRF only protects mutations).
///   2. Bypasses public paths (no session to bind against).
///   3. Bypasses [`auth::CSRF_EXEMPT_POST_PATHS`] (/login, /setup --
///      no established session yet, SameSite is the active defense).
///   4. For every other POST, buffers the body, parses
///      application/x-www-form-urlencoded, looks for `csrf_token`,
///      verifies it matches the session's token in constant time,
///      and re-builds the request with the buffered body so the
///      handler sees the original form payload unchanged.
///
/// Any mismatch returns 403 with a clean error page.
async fn csrf_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    use axum::body::Body;
    use axum::http::Method;

    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if auth::is_public_path(path) || auth::is_csrf_exempt(path) {
        return next.run(request).await;
    }
    if state.operators.is_none() {
        // Smoke-test surface: no auth, no CSRF.
        return next.run(request).await;
    }

    // Need the session to know which token to compare against.
    let cookie_header = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let session_id = cookie_header
        .as_deref()
        .and_then(auth::session_id_from_cookies);
    let Some(session) = (match session_id {
        Some(id) => state.sessions.get(&id).await,
        None => None,
    }) else {
        // No session = the auth middleware would have already
        // redirected. If we land here it means the request slipped
        // past auth somehow; reject defensively.
        return (StatusCode::FORBIDDEN, Html(render_csrf_failure())).into_response();
    };

    // Buffer the body so we can both inspect it AND re-feed it.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 1024 * 16).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, Html(render_csrf_failure())).into_response();
        }
    };

    let provided_token = parse_csrf_token_from_form(&bytes).unwrap_or_default();
    if !auth::csrf_tokens_match(&provided_token, &session.csrf_token) {
        tracing::warn!(
            path = %parts.uri.path(),
            username = %session.username,
            "CSRF token mismatch on POST request; rejecting with 403. \
             Likely causes: stale browser tab (form rendered against a previous \
             session), open-tab-after-logout, or an actual cross-origin attempt."
        );
        return (StatusCode::FORBIDDEN, Html(render_csrf_failure())).into_response();
    }

    // Reconstruct the request with the same buffered body so the
    // downstream handler sees the original payload.
    let new_request = axum::http::Request::from_parts(parts, Body::from(bytes));
    next.run(new_request).await
}

/// Parse `csrf_token` out of an `application/x-www-form-urlencoded`
/// body. Returns the first match; ignores any other fields.
fn parse_csrf_token_from_form(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    for pair in s.split('&') {
        let mut kv = pair.splitn(2, '=');
        let k = kv.next()?;
        if k == "csrf_token" {
            let v = kv.next().unwrap_or("");
            // form-urlencoded: + -> space, %xx -> byte. csrf tokens are
            // hex so neither character appears, but we still decode
            // to be safe.
            let decoded = url_decode_minimal(v);
            return Some(decoded);
        }
    }
    None
}

/// Minimal application/x-www-form-urlencoded decoder for the CSRF
/// token field. Handles `+` -> space + `%HH` percent escapes; assumes
/// UTF-8. Our tokens are hex so the decoder is overkill but the
/// safety net is cheap.
fn url_decode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(((h * 16 + l) as u8) as char);
                    i += 3;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Inline failure page for CSRF mismatch. Kept simple -- not localized
/// because operators landing here either hit it via stale-form refresh
/// (re-load the page) or via actual attack (the message is irrelevant).
fn render_csrf_failure() -> String {
    "<!DOCTYPE html><html><head><title>CSRF rejected</title></head>\
     <body style=\"font-family:sans-serif;padding:2rem;max-width:40rem;margin:0 auto;\">\
     <h1>CSRF rejected</h1>\
     <p>Your request did not carry a valid CSRF token. The most common cause is a \
     stale browser tab: the form was rendered against a previous session and the \
     token expired when you signed in again. Reload the page and re-submit.</p>\
     <p><a href=\"/\">Back to the landing page</a></p>\
     </body></html>"
        .to_string()
}

/// Build the axum router with an `AppState` attached. Every handler that
/// needs the store extracts it via `State<AppState>`.
pub fn router_with_state(state: AppState) -> Router {
    let auth_layer = axum::middleware::from_fn_with_state(state.clone(), auth_middleware);
    let permission_layer =
        axum::middleware::from_fn_with_state(state.clone(), permission_middleware);
    let csrf_layer = axum::middleware::from_fn_with_state(state.clone(), csrf_middleware);
    Router::new()
        .route("/", get(home_handler))
        .route("/components", get(components_handler))
        .route(
            "/install",
            get(install_hub_handler).post(install_all_handler),
        )
        .route(
            "/install/postgres",
            get(install_postgres_form_handler).post(install_postgres_handler),
        )
        .route(
            "/install/postgres/uninstall",
            get(uninstall_confirm_handler).post(uninstall_postgres_handler),
        )
        .route(
            "/install/kanidm",
            get(install_kanidm_form_handler).post(install_kanidm_handler),
        )
        .route(
            "/install/kanidm/uninstall",
            get(uninstall_kanidm_confirm_handler).post(uninstall_kanidm_handler),
        )
        .route(
            "/install/garage",
            get(install_garage_form_handler).post(install_garage_handler),
        )
        .route(
            "/install/garage/uninstall",
            get(uninstall_garage_confirm_handler).post(uninstall_garage_handler),
        )
        .route(
            "/install/openfga",
            get(install_openfga_form_handler).post(install_openfga_handler),
        )
        .route(
            "/install/openfga/uninstall",
            get(uninstall_openfga_confirm_handler).post(uninstall_openfga_handler),
        )
        .route(
            "/install/qdrant",
            get(install_qdrant_form_handler).post(install_qdrant_handler),
        )
        .route(
            "/install/qdrant/uninstall",
            get(uninstall_qdrant_confirm_handler).post(uninstall_qdrant_handler),
        )
        .route(
            "/install/greptime",
            get(install_greptime_form_handler).post(install_greptime_handler),
        )
        .route(
            "/install/greptime/uninstall",
            get(uninstall_greptime_confirm_handler).post(uninstall_greptime_handler),
        )
        .route(
            "/install/lakekeeper",
            get(install_lakekeeper_form_handler).post(install_lakekeeper_handler),
        )
        .route(
            "/install/lakekeeper/uninstall",
            get(uninstall_lakekeeper_confirm_handler).post(uninstall_lakekeeper_handler),
        )
        .route(
            "/install/databend",
            get(install_databend_form_handler).post(install_databend_handler),
        )
        .route(
            "/install/databend/uninstall",
            get(uninstall_databend_confirm_handler).post(uninstall_databend_handler),
        )
        .route(
            "/install/grafana",
            get(install_grafana_form_handler).post(install_grafana_handler),
        )
        .route(
            "/install/grafana/uninstall",
            get(uninstall_grafana_confirm_handler).post(uninstall_grafana_handler),
        )
        .route(
            "/install/restate",
            get(install_restate_form_handler).post(install_restate_handler),
        )
        .route(
            "/install/restate/uninstall",
            get(uninstall_restate_confirm_handler).post(uninstall_restate_handler),
        )
        .route("/install/{component}", get(install_component_handler))
        .route("/install/job/{id}", get(install_job_handler))
        .route("/install/job/{id}/rollback", post(install_rollback_handler))
        .route("/api/install/job/{id}", get(install_job_api_handler))
        .route("/admin/secrets", get(secrets_index_handler))
        .route("/admin/secrets/{name}/rotate", post(secrets_rotate_handler))
        .route("/status", get(status_handler))
        .route("/state", get(state_page_handler))
        .route("/resource/{kind}/{name}", get(resource_handler))
        .route(
            "/resource/{kind}/{name}/delete",
            post(resource_delete_handler),
        )
        .route("/login", get(login_form_handler).post(login_post_handler))
        .route("/setup", get(setup_form_handler).post(setup_post_handler))
        .route("/logout", post(logout_handler))
        .route("/account", get(account_handler))
        .route("/audit", get(audit_handler))
        .route(
            "/admin/operators",
            get(admin_operators_handler).post(admin_create_operator_handler),
        )
        .route(
            "/admin/operators/{name}/groups",
            post(admin_update_operator_groups_handler),
        )
        .route(
            "/admin/operators/{name}/delete",
            post(admin_delete_operator_handler),
        )
        .route("/admin/groups", get(admin_groups_handler))
        .route("/healthz", get(healthz_handler))
        .route("/api/state/info", get(state_info_handler))
        .route("/static/computeza.css", get(css_handler))
        .route("/static/brand/computeza-logo.svg", get(logo_handler))
        .route(
            "/static/brand/computeza-logo-mono-light.svg",
            get(logo_mono_light_handler),
        )
        .route("/static/brand/computeza-favicon.svg", get(favicon_handler))
        .route("/static/brand/computeza-pulse.json", get(lottie_handler))
        .route("/favicon.ico", get(favicon_handler))
        .layer(csrf_layer)
        .layer(permission_layer)
        .layer(auth_layer)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn install_hub_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let active = active_jobs(&state);
    Html(render_install_hub(&l, &active))
}

/// Snapshot of an in-flight install job, surfaced on the install hub
/// so the operator can re-attach the wizard after a browser refresh
/// or navigation away.
#[derive(Clone, Debug)]
pub struct ActiveJob {
    /// Opaque job id; used as the path component of `/install/job/{id}`.
    pub id: String,
    /// Slug of whichever component is currently running, if any.
    pub running_slug: Option<String>,
    /// How many components in the job's checklist have finished.
    pub components_done: usize,
    /// Total components in the job's checklist.
    pub components_total: usize,
}

/// Return an entry for every job whose snapshot says it hasn't
/// completed yet. Used by the install hub to surface "Install in
/// progress" banners that link back to the wizard.
fn active_jobs(state: &AppState) -> Vec<ActiveJob> {
    let map = state.jobs.lock().unwrap();
    map.iter()
        .filter_map(|(id, prog)| {
            let p = prog.lock().unwrap();
            if p.completed {
                return None;
            }
            let total = p.components.len();
            let done = p
                .components
                .iter()
                .filter(|c| {
                    matches!(
                        c.state,
                        computeza_driver_native::progress::ComponentState::Done
                    )
                })
                .count();
            let running = p
                .components
                .iter()
                .find(|c| {
                    matches!(
                        c.state,
                        computeza_driver_native::progress::ComponentState::Running
                    )
                })
                .map(|c| c.slug.clone());
            Some(ActiveJob {
                id: id.clone(),
                running_slug: running,
                components_done: done,
                components_total: total,
            })
        })
        .collect()
}

/// POST /install -- the unified whole-stack install. Parses one config
/// per available component out of the flat `<slug>__<field>` form,
/// spawns a single background job, and runs every install in
/// [`INSTALL_ORDER`] sequentially. A failure on any component stops
/// the chain (later components stay un-installed; earlier ones are
/// left in place).
async fn install_all_handler(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }

    let mut planned: Vec<(&'static str, InstallConfig)> = Vec::new();
    for slug in INSTALL_ORDER {
        if !is_available(slug) {
            continue;
        }
        match build_unified_config(&form, slug) {
            Ok(cfg) => planned.push((*slug, cfg)),
            Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
        }
    }

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);

    // Seed the per-component checklist up front so the wizard can
    // render the full N-row list with everything Pending immediately.
    let slugs: Vec<&'static str> = planned.iter().map(|(s, _)| *s).collect();
    progress.init_components(&slugs);

    tokio::spawn(async move {
        let mut overall = String::new();
        let total = planned.len();
        for (idx, (slug, config)) in planned.into_iter().enumerate() {
            progress.start_component(slug);
            let banner = format!("[{}/{}] Installing {slug}...", idx + 1, total);
            progress.set_message(banner);
            match dispatch_install(slug, &progress, &config).await {
                Ok((summary, _port, spec)) => {
                    progress.finish_component(slug, &summary);
                    // finalize_managed_install_after_success persists
                    // the spec + the InstallConfig + the admin
                    // credential (when applicable). Same helper the
                    // per-component install handlers call so install
                    // and rollback stay symmetric across both paths.
                    let summary = finalize_managed_install_after_success(
                        slug,
                        &config,
                        &spec,
                        summary,
                        store.as_deref(),
                        secrets.as_deref(),
                        &progress,
                    )
                    .await;
                    overall.push_str(&format!("=== {slug} ===\n{summary}\n\n"));
                }
                Err(detail) => {
                    progress.fail_component(slug, &detail);
                    progress.finish_failure(format!(
                        "{slug} install failed: {detail}\n\nProgress before the failure:\n{overall}\n\
                         Fix the underlying issue (see the log panel above) and re-submit the install form. \
                         Components that already installed are idempotent on re-run."
                    ));
                    return;
                }
            }
        }
        progress.finish_success(format!(
            "Installed {total} component(s) successfully.\n\n{overall}Visit /status for the live reconciler view."
        ));
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

/// Slugs whose drivers will eventually consume a per-instance admin
/// credential. The unified install generates a strong random password
/// for each one, stores it encrypted in the secrets store (when
/// attached), and surfaces it on the result page exactly once.
///
/// v0.0.x drivers don't *apply* these passwords yet (postgres still
/// uses loopback trust; kanidm uses `kanidmd recover_account`; grafana
/// uses its default `admin` user). The credentials are pre-provisioned
/// so the operator has them on hand when the driver wiring lands in a
/// follow-up.
const COMPONENTS_WITH_ADMIN_CREDENTIAL: &[(&str, &str, &str)] = &[
    ("postgres", "postgres", "superuser password"),
    ("kanidm", "admin", "initial admin password"),
    ("grafana", "admin", "initial admin password"),
];

/// Persist a per-slug [`InstallConfig`] under the metadata-store kind
/// `install-config` with name `<slug>-local`. Used by the unified
/// install so rollback / repair can target the operator's chosen
/// service name + root dir rather than blindly using driver defaults.
///
/// Upsert: on revision conflict we load the existing entry's revision
/// and retry the save once. Returns `Err` only on the second failure.
async fn save_install_config(
    store: &SqliteStore,
    slug: &str,
    config: &InstallConfig,
) -> anyhow::Result<()> {
    let key = ResourceKey::cluster_scoped("install-config", format!("{slug}-local"));
    let value = serde_json::to_value(config)
        .map_err(|e| anyhow::anyhow!("serialising InstallConfig for {slug}: {e}"))?;
    let expected_revision = match store.load(&key).await {
        Ok(Some(existing)) => Some(existing.revision),
        _ => None,
    };
    store
        .save(&key, &value, expected_revision)
        .await
        .map_err(|e| anyhow::anyhow!("save install-config/{slug}-local: {e}"))?;
    Ok(())
}

/// Read back the per-slug [`InstallConfig`] persisted by
/// [`save_install_config`]. Returns `None` when no row exists (e.g.
/// the install ran before this persistence layer landed, or via the
/// per-component pages that don't yet write install-config rows).
async fn load_install_config(store: &SqliteStore, slug: &str) -> Option<InstallConfig> {
    let key = ResourceKey::cluster_scoped("install-config", format!("{slug}-local"));
    match store.load(&key).await {
        Ok(Some(stored)) => serde_json::from_value::<InstallConfig>(stored.spec)
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    component = slug,
                    "install-config/{slug}-local exists but is not a valid InstallConfig; \
                     falling back to driver defaults"
                );
                e
            })
            .ok(),
        _ => None,
    }
}

/// Delete the persisted install-config for a slug. Called from
/// rollback so the metadata store doesn't keep a stale row pointing
/// at a service that no longer exists.
async fn delete_install_config(store: &SqliteStore, slug: &str) {
    let key = ResourceKey::cluster_scoped("install-config", format!("{slug}-local"));
    if let Err(e) = store.delete(&key, None).await {
        tracing::debug!(
            error = %e,
            component = slug,
            "delete install-config/{slug}-local: row may not have existed (this is fine on legacy installs)"
        );
    }
}

/// Shared post-install work for the per-component install handlers
/// (and reusable by the unified install). Persists the spec under
/// `<slug>-instance/local`, persists the `InstallConfig` under
/// `install-config/<slug>-local`, and generates the admin credential
/// for components that have one (postgres / kanidm / grafana) --
/// storing the credential encrypted in the secrets store and pushing
/// it onto the progress handle for one-time display on the result
/// page.
///
/// Mirrors `teardown_managed_uninstall` so install and uninstall stay
/// symmetric: anything written here is dropped there.
///
/// Returns the (possibly augmented) summary string; the caller passes
/// it to `progress.finish_success`. Best-effort throughout -- a
/// failed metadata write leaves the on-disk service running and
/// appends a `Note:` line to the summary.
async fn finalize_managed_install_after_success(
    slug: &str,
    config: &InstallConfig,
    spec: &serde_json::Value,
    mut summary: String,
    store: Option<&SqliteStore>,
    secrets: Option<&SecretsStore>,
    progress: &ProgressHandle,
) -> String {
    if let Some(store) = store {
        let kind = format!("{slug}-instance");
        let key = ResourceKey::cluster_scoped(&kind, "local");
        let expected_revision = match store.load(&key).await {
            Ok(Some(existing)) => Some(existing.revision),
            _ => None,
        };
        match store.save(&key, spec, expected_revision).await {
            Ok(_) => summary.push_str(&format!(
                "\n\nRegistered as {slug}-instance/local in the metadata store.\nVisit /status to see it.",
            )),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    component = slug,
                    "install: store.save failed for {slug}-instance/local; \
                     the on-disk service is fine. Visit /status or re-run install to retry."
                );
                summary.push_str(&format!(
                    "\n\nNote: did not register {slug}-instance/local ({e}). Visit /status to inspect."
                ));
            }
        }
        if let Err(e) = save_install_config(store, slug, config).await {
            tracing::warn!(
                error = %e,
                component = slug,
                "install: failed to persist install-config/{slug}-local; \
                 rollback / repair on this row will fall back to driver defaults."
            );
        }
    }

    if let Some((_, username, label)) = COMPONENTS_WITH_ADMIN_CREDENTIAL
        .iter()
        .find(|(s, _, _)| *s == slug)
        .copied()
    {
        let password = generate_random_password();
        let secret_ref = format!("{slug}/admin-password");
        if let Some(secrets) = secrets {
            if let Err(e) = secrets.put(&secret_ref, &password).await {
                tracing::warn!(
                    error = %e,
                    component = slug,
                    "install: secrets.put failed for {secret_ref}; \
                     the credential will only appear in-band on the result page."
                );
            }
        }
        progress.push_credential(computeza_driver_native::progress::GeneratedCredential {
            component: slug.to_string(),
            label: label.to_string(),
            value: password,
            username: Some(username.to_string()),
            secret_ref: Some(secret_ref),
        });
    }

    summary
}

/// Shared teardown for the per-component uninstall handlers. Loads
/// the persisted `InstallConfig` so the uninstall targets the
/// operator's chosen service name + root dir (instead of falling back
/// to driver defaults), then drops the `<slug>-instance/local` row,
/// the `install-config/<slug>-local` row, and the
/// `<slug>/admin-password` secret. Pairs with the unified install's
/// `finalize_managed_install` pattern.
///
/// Returns the uninstall summary (or the dispatcher error) so the
/// caller can render the result page.
async fn teardown_managed_uninstall(
    slug: &str,
    store: Option<&SqliteStore>,
    secrets: Option<&SecretsStore>,
) -> Result<String, String> {
    let config = match store {
        Some(s) => load_install_config(s, slug).await.unwrap_or_default(),
        None => InstallConfig::default(),
    };
    let result = dispatch_uninstall_with_config(slug, &config).await;

    if let Some(store) = store {
        let kind = format!("{slug}-instance");
        let key = ResourceKey::cluster_scoped(&kind, "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                component = slug,
                "uninstall: store.delete({slug}-instance/local) failed; \
                 row may linger -- inspect via /resource and clean up manually"
            );
        }
        delete_install_config(store, slug).await;
    }
    if let Some(secrets) = secrets {
        let secret_ref = format!("{slug}/admin-password");
        if let Err(e) = secrets.delete(&secret_ref).await {
            tracing::debug!(
                error = %e,
                component = slug,
                "uninstall: secrets.delete({secret_ref}) failed; \
                 ignoring -- the entry may not have existed"
            );
        }
    }
    result
}

/// Generate a 96-bit random password rendered as a 24-character hex
/// string. Hex avoids alphabet-modulo bias and is safe to paste into
/// shell prompts / config files (no quoting / escaping needed).
fn generate_random_password() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    use aes_gcm::aead::OsRng;
    let mut buf = [0u8; 12];
    OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(24);
    for b in &buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Look up whether a slug is marked `available: true` in [`COMPONENTS`].
/// Used by [`install_all_handler`] to skip pinned-but-unshippable
/// entries (xtable today) without erroring.
fn is_available(slug: &str) -> bool {
    COMPONENTS
        .iter()
        .find(|c| c.slug == slug)
        .map(|c| c.available)
        .unwrap_or(false)
}

/// Extract one component's [`InstallConfig`] from a unified form.
/// Field names follow the `<slug>__<field>` convention so a single
/// flat HashMap holds every component's inputs.
fn build_unified_config(
    form: &HashMap<String, String>,
    slug: &str,
) -> Result<InstallConfig, String> {
    let get = |k: &str| -> &str {
        form.get(&format!("{slug}__{k}"))
            .map(String::as_str)
            .unwrap_or("")
            .trim()
    };

    let version = match get("version") {
        "" => None,
        s => Some(s.to_string()),
    };
    let port = match get("port") {
        "" => None,
        s => Some(s.parse::<u16>().map_err(|_| {
            format!("{slug} port must be an integer between 1 and 65535; got {s:?}")
        })?),
    };
    let root_dir = match get("root_dir") {
        "" => None,
        s => Some(s.to_string()),
    };
    let service_name = match get("service_name") {
        "" => None,
        s if s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Some(s.to_string())
        }
        s => {
            return Err(format!(
                "{slug} service name {s:?} must be ASCII alphanumeric / hyphen / underscore only"
            ));
        }
    };
    Ok(InstallConfig {
        version,
        port,
        root_dir,
        service_name,
    })
}

/// Apply per-slug [`InstallConfig`] overrides to the `unit_name` +
/// `root_dir` fields shared by every component's UninstallOptions
/// struct. No-op when individual fields are unset (operator accepted
/// driver defaults at install time).
#[cfg(target_os = "linux")]
fn apply_uninstall_config_overrides(
    config: &InstallConfig,
    unit_name: &mut String,
    root_dir: &mut std::path::PathBuf,
) {
    if let Some(s) = &config.service_name {
        *unit_name = format!("{s}.service");
    }
    if let Some(d) = &config.root_dir {
        *root_dir = std::path::PathBuf::from(d);
    }
}

/// Custom-config variant of [`dispatch_uninstall`]. Used by the
/// unified rollback flow so an operator who installed with custom
/// `service_name` / `root_dir` doesn't end up with default-named
/// services lingering after the rollback. Linux-only -- the v0.0.x
/// install path is Linux-only anyway, so the non-Linux branch
/// returns a clean error.
async fn dispatch_uninstall_with_config(
    slug: &str,
    config: &InstallConfig,
) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux;
        match slug {
            "postgres" => {
                let mut opts = linux::postgres::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::postgres::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "kanidm" => {
                let mut opts = linux::kanidm::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::kanidm::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "garage" => {
                let mut opts = linux::garage::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::garage::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "openfga" => {
                let mut opts = linux::openfga::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::openfga::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "qdrant" => {
                let mut opts = linux::qdrant::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::qdrant::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "lakekeeper" => {
                let mut opts = linux::lakekeeper::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::lakekeeper::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "greptime" => {
                let mut opts = linux::greptime::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::greptime::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "grafana" => {
                let mut opts = linux::grafana::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::grafana::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "restate" => {
                let mut opts = linux::restate::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::restate::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "databend" => {
                let mut opts = linux::databend::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::databend::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            other => Err(format!(
                "dispatch_uninstall_with_config: unknown component slug {other:?}"
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (slug, config);
        Err(
            "dispatch_uninstall_with_config: v0.0.x install path is Linux-only; \
             rollback with custom config is not available on this platform"
                .into(),
        )
    }
}

/// Run the install for one component and return `(summary, port, spec)`
/// where `spec` is the JSON shape the metadata store wants under
/// `<slug>-instance/local`. The per-component spec shapes mirror what
/// the existing per-slug handlers (e.g. `install_postgres_handler`)
/// write today.
async fn dispatch_install(
    slug: &str,
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16, serde_json::Value), String> {
    let (summary, port) = match slug {
        "postgres" => run_postgres_install_with_progress(progress, config).await?,
        "kanidm" => run_kanidm_install_with_progress(progress, config).await?,
        "garage" => run_garage_install_with_progress(progress, config).await?,
        "openfga" => run_openfga_install_with_progress(progress, config).await?,
        "qdrant" => run_qdrant_install_with_progress(progress, config).await?,
        "lakekeeper" => run_lakekeeper_install_with_progress(progress, config).await?,
        "greptime" => run_greptime_install_with_progress(progress, config).await?,
        "grafana" => run_grafana_install_with_progress(progress, config).await?,
        "restate" => run_restate_install_with_progress(progress, config).await?,
        "databend" => run_databend_install_with_progress(progress, config).await?,
        other => {
            return Err(format!(
                "dispatch_install: unknown component slug {other:?}"
            ))
        }
    };
    let spec = match slug {
        "postgres" => serde_json::json!({
            "endpoint": {
                "host": "127.0.0.1",
                "port": port,
                "superuser": "postgres",
            },
            "superuser_password_ref": "postgres/admin-password",
            "databases": [],
            "prune": false,
        }),
        "kanidm" => serde_json::json!({
            "endpoint": {
                "base_url": format!("https://127.0.0.1:{port}"),
                "insecure_skip_tls_verify": true,
            },
        }),
        "garage" => {
            let admin = port + 3;
            serde_json::json!({
                "endpoint": {
                    "base_url": format!("http://127.0.0.1:{admin}"),
                    "insecure_skip_tls_verify": false,
                },
            })
        }
        _ => serde_json::json!({
            "endpoint": {
                "base_url": format!("http://127.0.0.1:{port}"),
                "insecure_skip_tls_verify": false,
            },
        }),
    };
    Ok((summary, port, spec))
}

async fn install_postgres_form_handler() -> Html<String> {
    let l = Localizer::english();
    let detected = computeza_driver_native::detect::postgres().await;
    Html(render_install(&l, &detected))
}

async fn install_component_handler(Path(component): Path<String>) -> Html<String> {
    let l = Localizer::english();
    // Postgres lives at `/install/postgres` with its own route; this
    // catch-all handles the other 10 entries on the install hub.
    Html(render_install_coming_soon(&l, &component))
}

#[derive(serde::Deserialize)]
struct InstallForm {
    /// Component slug. v0.0.x recognises only "postgres".
    component: String,
    /// Version string from the per-component dropdown (e.g. "18.3-1",
    /// "17.9-1"). Empty / unknown values resolve to the driver's
    /// "latest" default.
    #[serde(default)]
    version: String,
    /// TCP port to listen on. Empty string means "use the driver default".
    #[serde(default)]
    port: String,
    /// Override the data + cluster root. Empty string means use the
    /// platform default under %PROGRAMDATA% / /var/lib / Application Support.
    #[serde(default)]
    root_dir: String,
    /// Service name registered with the OS service manager. Empty
    /// string means use the driver default (`computeza-postgres`).
    #[serde(default)]
    service_name: String,
}

/// Parsed user-facing install configuration. Each field defaults to
/// the driver default when the corresponding form field was blank.
///
/// Serializable so the unified install can persist a per-slug copy
/// alongside each component's spec, letting the rollback / repair
/// flows target the same service name + root dir the operator
/// originally chose (rather than blindly using driver defaults).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct InstallConfig {
    /// Version pin from the form's dropdown / text input. `None`
    /// resolves to the driver's "latest" default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// TCP port to bind the service on. `None` uses the driver
    /// default (e.g. 5432 for postgres, 8443 for kanidm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Override the install root. `None` uses the platform default
    /// (`/var/lib/computeza/<slug>` on Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
    /// Service name registered with the OS service manager. `None`
    /// uses the driver default (`computeza-<slug>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
}

impl InstallForm {
    fn into_config(self) -> Result<InstallConfig, String> {
        let version = match self.version.trim() {
            "" => None,
            s => Some(s.to_string()),
        };
        let port =
            match self.port.trim() {
                "" => None,
                s => Some(s.parse::<u16>().map_err(|_| {
                    format!("port must be an integer between 1 and 65535; got {s:?}")
                })?),
            };
        let root_dir = match self.root_dir.trim() {
            "" => None,
            s => Some(s.to_string()),
        };
        let service_name = match self.service_name.trim() {
            "" => None,
            s if s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                Some(s.to_string())
            }
            s => {
                return Err(format!(
                    "service name {s:?} must be ASCII alphanumeric / hyphen / underscore only"
                ))
            }
        };
        Ok(InstallConfig {
            version,
            port,
            root_dir,
            service_name,
        })
    }
}

async fn install_postgres_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "postgres" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => {
            return Html(render_install_result(&l, false, &msg)).into_response();
        }
    };

    // Mint a job id, store an empty progress object, spawn the install
    // in the background, and redirect the browser to the wizard page
    // that polls for updates.
    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_postgres_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "host": "127.0.0.1",
                        "port": port,
                        "superuser": "postgres",
                    },
                    "superuser_password_ref": "postgres/admin-password",
                    "databases": [],
                    "prune": false,
                });
                let summary = finalize_managed_install_after_success(
                    "postgres",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;

                // Kick a single observe() immediately so /status doesn't
                // sit on the previous (likely failed) tick result for up
                // to 30 seconds. Best-effort: the periodic tick will
                // retry regardless.
                if let Some(store) = &store {
                    if let Err(e) =
                        observe_postgres_instance_now(store.clone(), secrets.clone(), &spec).await
                    {
                        tracing::debug!(
                            error = %e,
                            "post-install observe attempt did not write a fresh status; \
                             /status will update on the next periodic tick"
                        );
                    }
                }
                progress.finish_success(summary);
            }
            Err(detail) => {
                progress.finish_failure(detail);
            }
        }
    });

    // 303 See Other to the wizard page. Standard POST-redirect-GET so
    // refreshing the wizard doesn't re-submit the form.
    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_confirm(&l))
}

async fn uninstall_postgres_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    // teardown_managed_uninstall loads install-config/postgres-local
    // (when one was persisted by the unified install) and uses it to
    // build UninstallOptions so the teardown targets the operator's
    // chosen service name + root dir. It also drops the
    // postgres-instance/local row, the install-config row, and the
    // postgres/admin-password secret.
    let result =
        teardown_managed_uninstall("postgres", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

// ============================================================
// Kanidm install path -- code remains in tree but routes are
// disabled while the hub card is `available: false`. See the
// commented-out kanidm routes in `router_with_state`.
// ============================================================
async fn install_kanidm_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_kanidm(&l))
}

async fn install_kanidm_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "kanidm" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_kanidm_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("https://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": true,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "kanidm",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_kanidm_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_kanidm_confirm(&l))
}

async fn uninstall_kanidm_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("kanidm", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_kanidm_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::kanidm;
    let mut opts = kanidm::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match kanidm::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nport: {}\nkanidm CLI symlink: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.cli_symlink
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not created)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "macos")]
async fn run_kanidm_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::macos::kanidm;
    let mut opts = kanidm::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.label = format!("com.computeza.{s}");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match kanidm::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nplist_path: {}\nport: {}\nkanidm CLI symlink: {}",
                r.bin_dir.display(),
                r.plist_path.display(),
                r.port,
                r.cli_symlink
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not created)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "windows")]
async fn run_kanidm_install_with_progress(
    progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::windows::kanidm;
    match kanidm::install_with_progress(kanidm::InstallOptions::default(), progress).await {
        Ok(_) => Ok(("kanidm installed".into(), kanidm::DEFAULT_PORT)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn run_kanidm_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("install is supported on Linux, macOS, and Windows only".into())
}

// ============================================================
// Garage install path
// ============================================================

async fn install_garage_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_garage(&l))
}

async fn install_garage_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "garage" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_garage_install_with_progress(&progress, &config).await {
            Ok((summary, s3_port)) => {
                // Reconciler hits the admin API at s3_port + 3
                // (3903 with the canonical 3900 layout).
                let admin_port = s3_port + 3;
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{admin_port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "garage",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_garage_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_garage_confirm(&l))
}

async fn uninstall_garage_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("garage", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_garage_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::garage;
    let mut opts = garage::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match garage::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nS3 port: {}\nadmin port: {}\ngarage CLI symlink: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.port + 3,
                r.cli_symlink
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not created)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_garage_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Garage install requires a supported Linux host. v0.0.x does not yet ship a macOS or Windows driver for this component.".into())
}

// ============================================================
// OpenFGA install path
// ============================================================

async fn install_openfga_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_openfga(&l))
}

async fn install_openfga_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "openfga" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_openfga_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "openfga",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_openfga_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_openfga_confirm(&l))
}

async fn uninstall_openfga_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("openfga", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_openfga_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::openfga;
    let mut opts = openfga::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match openfga::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nHTTP port: {}\ngRPC port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.port + 1,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_openfga_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err(
        "OpenFGA install requires a supported Linux host. v0.0.x ships only the Linux driver."
            .into(),
    )
}

// ============================================================
// Qdrant install path
// ============================================================

async fn install_qdrant_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_qdrant(&l))
}

async fn install_qdrant_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "qdrant" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_qdrant_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "qdrant",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_qdrant_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_qdrant_confirm(&l))
}

async fn uninstall_qdrant_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("qdrant", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_qdrant_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::qdrant;
    let mut opts = qdrant::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match qdrant::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nREST port: {}\ngRPC port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.port + 1,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_qdrant_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err(
        "Qdrant install requires a supported Linux host. v0.0.x ships only the Linux driver."
            .into(),
    )
}

// ============================================================
// GreptimeDB install path
// ============================================================

async fn install_greptime_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_greptime(&l))
}

async fn install_greptime_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "greptime" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_greptime_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "greptime",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_greptime_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_greptime_confirm(&l))
}

async fn uninstall_greptime_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("greptime", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_greptime_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::greptime;
    let mut opts = greptime::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match greptime::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nHTTP port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_greptime_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("GreptimeDB install requires a supported Linux host.".into())
}

// ============================================================
// Lakekeeper install path
// ============================================================

async fn install_lakekeeper_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_lakekeeper(&l))
}

async fn install_lakekeeper_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "lakekeeper" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_lakekeeper_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "lakekeeper",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_lakekeeper_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_lakekeeper_confirm(&l))
}

async fn uninstall_lakekeeper_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result = teardown_managed_uninstall(
        "lakekeeper",
        state.store.as_deref(),
        state.secrets.as_deref(),
    )
    .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_lakekeeper_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::lakekeeper;
    let mut opts = lakekeeper::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match lakekeeper::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nREST port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_lakekeeper_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Lakekeeper install requires a supported Linux host.".into())
}

// ============================================================
// Databend install path
// ============================================================

async fn install_databend_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_databend(&l))
}

async fn install_databend_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "databend" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_databend_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "databend",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_databend_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_databend_confirm(&l))
}

async fn uninstall_databend_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("databend", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_databend_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::databend;
    let mut opts = databend::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match databend::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nHTTP port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_databend_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Databend install requires a supported Linux host.".into())
}

// ============================================================
// Grafana install path
// ============================================================

async fn install_grafana_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_grafana(&l))
}

async fn install_grafana_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "grafana" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_grafana_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "grafana",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_grafana_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_grafana_confirm(&l))
}

async fn uninstall_grafana_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("grafana", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_grafana_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::grafana;
    let mut opts = grafana::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match grafana::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nHTTP port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_grafana_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Grafana install requires a supported Linux host.".into())
}

// ============================================================
// Restate install path
// ============================================================

async fn install_restate_form_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_restate(&l))
}

async fn install_restate_handler(
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let l = Localizer::english();
    if let Err(resp) = guard_supported_os(&l) {
        return resp;
    }
    if form.component != "restate" {
        return Html(render_install_result(
            &l,
            false,
            &format!("unknown component: {}", form.component),
        ))
        .into_response();
    }
    let config = match form.into_config() {
        Ok(c) => c,
        Err(msg) => return Html(render_install_result(&l, false, &msg)).into_response(),
    };

    let job_id = Uuid::new_v4().to_string();
    let progress_state = Arc::new(StdMutex::new(InstallProgress::default()));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), progress_state.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_restate_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let spec = serde_json::json!({
                    "endpoint": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "insecure_skip_tls_verify": false,
                    },
                });
                let summary = finalize_managed_install_after_success(
                    "restate",
                    &config,
                    &spec,
                    summary,
                    store.as_deref(),
                    secrets.as_deref(),
                    &progress,
                )
                .await;
                progress.finish_success(summary);
            }
            Err(detail) => progress.finish_failure(detail),
        }
    });

    Redirect303(format!("/install/job/{job_id}")).into_response()
}

async fn uninstall_restate_confirm_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_uninstall_restate_confirm(&l))
}

async fn uninstall_restate_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let result =
        teardown_managed_uninstall("restate", state.store.as_deref(), state.secrets.as_deref())
            .await;
    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
}

#[cfg(target_os = "linux")]
async fn run_restate_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::restate;
    let mut opts = restate::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match restate::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\nunit_path: {}\nIngress port: {}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_restate_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Restate install requires a supported Linux host.".into())
}

/// Build the data-dir placeholder for the form. Suffixes the leaf
/// onto the per-OS root so the operator sees the full path they'd
/// get from accepting the default.
fn root_dir_placeholder_for_leaf(leaf: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        let programdata = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        format!("{programdata}\\Computeza\\{leaf}")
    }
    #[cfg(target_os = "linux")]
    {
        format!("/var/lib/computeza/{leaf}")
    }
    #[cfg(target_os = "macos")]
    {
        format!("/Library/Application Support/Computeza/{leaf}")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        format!("./{leaf}")
    }
}

/// One entry in the version dropdown. `value` is the string the form
/// posts back; `label` is the display string. Sorted latest-first.
#[derive(Clone, Debug)]
struct VersionOption {
    value: String,
    label: String,
}

/// Available pinned bundle versions for the postgres install path.
/// Windows has the autonomous-download bundles; Linux/macOS still rely
/// on the host package manager, so we offer a single "(host-installed)"
/// option that maps to `version = None` in the spec.
fn postgres_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "windows")]
    {
        use computeza_driver_native::windows::postgres;
        let mut out = Vec::new();
        for (i, b) in postgres::available_versions().iter().enumerate() {
            let suffix = if i == 0 { " (latest)" } else { "" };
            out.push(VersionOption {
                value: b.version.to_string(),
                label: format!("PostgreSQL {}{}", b.version, suffix),
            });
        }
        out
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "host-installed PostgreSQL".into(),
        }]
    }
}

/// Format an `Uninstalled`-shaped result (steps + warnings vecs) into
/// the text block shown on the install-result page. Per-OS variants
/// all converge on this format so the UI is OS-agnostic. Used by the
/// Linux-only `dispatch_uninstall_with_config` -- gated cfg-wise so
/// non-Linux builds (which don't have that dispatch) don't see this
/// as dead code.
#[cfg(target_os = "linux")]
fn format_uninstall_summary(steps: &[String], warnings: &[String]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&format!("OK    {s}\n"));
    }
    for w in warnings {
        out.push_str(&format!("warn  {w}\n"));
    }
    if warnings.is_empty() {
        out.push_str("\nAll teardown steps completed cleanly.");
    } else {
        out.push_str(
            "\nSome teardown steps reported warnings (see above). \
             They are non-fatal -- re-run the uninstall to retry any \
             step that left state behind.",
        );
    }
    out
}

/// Construct a postgres reconciler for the freshly-installed instance
/// and run a single `observe()` so the persisted status snapshot is
/// up to date by the time the wizard's redirect lands on `/status`.
///
/// Best-effort: if the spec doesn't parse or the connection fails, we
/// log and return -- the periodic tick will retry on its next 30-second
/// interval. Non-fatal to the install.
async fn observe_postgres_instance_now(
    store: Arc<SqliteStore>,
    secrets: Option<Arc<SecretsStore>>,
    spec_json: &serde_json::Value,
) -> anyhow::Result<()> {
    use computeza_core::reconciler::Context;
    use computeza_core::{NoOpDriver, Reconciler};
    use computeza_reconciler_postgres::{PostgresReconciler, PostgresSpec};

    let mut spec: PostgresSpec = serde_json::from_value(spec_json.clone())?;
    hydrate_postgres_password(&mut spec, secrets.as_deref()).await;
    let reconciler: PostgresReconciler<NoOpDriver> =
        PostgresReconciler::new(spec.endpoint.clone(), spec.superuser_password)
            .with_state(store, "local");
    // observe() is "best effort writes a status row to the store".
    // It returns Ok with `last_observe_failed=true` on connection
    // failure, so the .await? here only propagates programming bugs.
    let _ = reconciler.observe(&Context::default()).await;
    Ok(())
}

/// Resolve `PostgresSpec::superuser_password_ref` against the secrets
/// store, populating `superuser_password` in place when the ref is
/// present and the lookup succeeds. Silently leaves the password as
/// the (empty) `SecretString` default when:
///
/// - the spec has no `superuser_password_ref`, or
/// - no secrets store is attached (operator hasn't set
///   `COMPUTEZA_SECRETS_PASSPHRASE`), or
/// - the lookup misses (the ref points at a name that doesn't exist
///   in the store).
///
/// For v0.0.x the empty-password fallback is fine because every
/// reconciler runs against a loopback-trust server. Once non-loopback
/// auth lands the missing-secret case will surface as a connection
/// error from the reconciler's own observe loop, not as a hard panic
/// here.
pub async fn hydrate_postgres_password(
    spec: &mut computeza_reconciler_postgres::PostgresSpec,
    secrets: Option<&SecretsStore>,
) {
    let Some(secrets) = secrets else { return };
    let Some(name) = spec.superuser_password_ref.as_deref() else {
        return;
    };
    match secrets.get(name).await {
        Ok(Some(value)) => {
            use secrecy::ExposeSecret;
            spec.superuser_password = secrecy::SecretString::from(value.expose_secret().to_owned());
            tracing::debug!(
                ref_name = name,
                "resolved postgres superuser_password_ref against secrets store"
            );
        }
        Ok(None) => tracing::warn!(
            ref_name = name,
            "postgres superuser_password_ref points at a name that is not in the secrets store; \
             falling back to empty password (loopback-trust default). Re-run the install or \
             insert the secret manually via the rotate UI to fix."
        ),
        Err(e) => tracing::warn!(
            error = %e,
            ref_name = name,
            "secrets store lookup failed for postgres superuser_password_ref; \
             falling back to empty password"
        ),
    }
}

/// 303 See Other redirect helper. axum has `Redirect::to` but it
/// defaults to 307 / 302 depending on version; we want the explicit
/// POST-redirect-GET semantics of 303.
struct Redirect303(String);

impl IntoResponse for Redirect303 {
    fn into_response(self) -> Response {
        (StatusCode::SEE_OTHER, [(header::LOCATION, self.0)]).into_response()
    }
}

/// GET /install/job/{id} -- the wizard page with the progress bars
/// and the JS poller. Re-renders the final result page once the
/// background task signals completion.
async fn install_job_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> (StatusCode, Html<String>) {
    let l = Localizer::english();
    // Look up the job state without cloning the inner Arc so we can
    // drain generated_credentials on the live state below.
    let job_arc = state.jobs.lock().unwrap().get(&job_id).cloned();
    let snapshot = job_arc.as_ref().map(|s| s.lock().unwrap().clone());
    match snapshot {
        None => (
            StatusCode::NOT_FOUND,
            Html(render_install_result(
                &l,
                false,
                &format!("Unknown install job: {job_id}"),
            )),
        ),
        Some(p) if p.completed => {
            // Drain credentials from the LIVE state so a refresh of
            // this URL renders the result page without the plaintext
            // password block. We do this even on the failure path
            // (some components may have generated credentials before
            // a later component failed -- the operator still needs
            // to capture them).
            let credentials = job_arc
                .as_ref()
                .map(|s| std::mem::take(&mut s.lock().unwrap().generated_credentials))
                .unwrap_or_default();

            // Show the rollback button whenever at least one component
            // in the job successfully installed -- failure runs benefit
            // from rolling back partial state, and successful runs may
            // want to undo the whole stack.
            let rollback_id = if p.components.iter().any(|c| {
                matches!(
                    c.state,
                    computeza_driver_native::progress::ComponentState::Done
                )
            }) {
                Some(job_id.as_str())
            } else {
                None
            };

            if let Some(err) = &p.error {
                (
                    StatusCode::OK,
                    Html(render_install_result_with_credentials(
                        &l,
                        false,
                        err,
                        &credentials,
                        rollback_id,
                    )),
                )
            } else {
                let summary = p.success_summary.clone().unwrap_or_default();
                (
                    StatusCode::OK,
                    Html(render_install_result_with_credentials(
                        &l,
                        true,
                        &summary,
                        &credentials,
                        rollback_id,
                    )),
                )
            }
        }
        Some(p) => (
            StatusCode::OK,
            Html(render_install_progress(&l, &job_id, &p)),
        ),
    }
}

/// GET /admin/secrets -- list every secret in the encrypted store.
/// Names only; values stay encrypted on disk and are never surfaced
/// from this page. Per-row Rotate button posts to
/// `/admin/secrets/{name}/rotate` which replaces the value and shows
/// the new value exactly once.
async fn secrets_index_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let names = match &state.secrets {
        Some(s) => match s.list_names().await {
            Ok(mut n) => {
                n.sort();
                Some(n)
            }
            Err(e) => {
                tracing::warn!(error = %e, "secrets list_names failed");
                Some(Vec::new())
            }
        },
        None => None,
    };
    Html(render_secrets_index(&l, names.as_deref()))
}

/// POST /admin/secrets/{name}/rotate -- replace the named secret's
/// value with a freshly generated 96-bit random hex string, show the
/// new value to the operator exactly once. Returns 404 if no secrets
/// store is attached or the name doesn't exist.
async fn secrets_rotate_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let l = Localizer::english();
    let Some(secrets) = &state.secrets else {
        return (
            StatusCode::NOT_FOUND,
            Html(render_install_result(
                &l,
                false,
                "No secrets store is attached on this server. Set COMPUTEZA_SECRETS_PASSPHRASE and restart `computeza serve`.",
            )),
        )
            .into_response();
    };
    match secrets.get(&name).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html(render_install_result(
                    &l,
                    false,
                    &format!("Secret not found: {name}"),
                )),
            )
                .into_response();
        }
        Err(e) => {
            return Html(render_install_result(
                &l,
                false,
                &format!("Reading secret {name} failed: {e}"),
            ))
            .into_response();
        }
        Ok(Some(_)) => {}
    }
    let new_value = generate_random_password();
    if let Err(e) = secrets.put(&name, &new_value).await {
        return Html(render_install_result(
            &l,
            false,
            &format!("Rotating secret {name} failed: {e}"),
        ))
        .into_response();
    }
    tracing::info!(name = %name, "secret rotated; new value surfaced on the rotate result page once");
    Html(render_secret_rotated(&l, &name, &new_value)).into_response()
}

/// POST /install/job/{id}/rollback -- uninstall every component the
/// job successfully installed, in reverse dependency order. Used to
/// roll back a partial-success run (some components installed, then
/// one failed) or to fully tear down a successful run.
///
/// Reads the job's components checklist, picks slugs with
/// `state == Done`, and dispatches each through the existing
/// per-component uninstall path. Errors during teardown are logged
/// and surfaced in the summary but do NOT stop the rollback -- best
/// effort tear-down across the board.
async fn install_rollback_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Response {
    let l = Localizer::english();
    let snapshot = state
        .jobs
        .lock()
        .unwrap()
        .get(&job_id)
        .map(|s| s.lock().unwrap().clone());
    let Some(p) = snapshot else {
        return Html(render_install_result(
            &l,
            false,
            &format!("Unknown install job: {job_id}"),
        ))
        .into_response();
    };

    if !p.completed {
        return Html(render_install_result(
            &l,
            false,
            "Cannot roll back a job that is still running. Wait for it to complete (the wizard \
             redirects to the result page automatically) and try again, or kill the server if \
             the job is stuck and re-roll-back after restart.",
        ))
        .into_response();
    }

    let installed: Vec<String> = p
        .components
        .iter()
        .filter(|c| {
            matches!(
                c.state,
                computeza_driver_native::progress::ComponentState::Done
            )
        })
        .map(|c| c.slug.clone())
        .collect();
    if installed.is_empty() {
        return Html(render_install_result(
            &l,
            false,
            "Nothing to roll back: this job's components vec contains no Done entries.",
        ))
        .into_response();
    }

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let mut summary = String::new();
    let mut any_failed = false;
    // Reverse dependency order so consumers come down before their
    // dependencies (e.g. lakekeeper before postgres).
    for slug in installed.iter().rev() {
        summary.push_str(&format!("=== rollback: {slug} ===\n"));

        // Load the install-config the operator chose at install time
        // (custom service_name / root_dir / etc) so the rollback
        // targets the SAME service the install created, not the
        // driver default. Falls back to InstallConfig::default() if
        // no config was persisted (legacy installs or per-component
        // install path that doesn't persist).
        let config = if let Some(store) = &store {
            load_install_config(store.as_ref(), slug)
                .await
                .unwrap_or_default()
        } else {
            InstallConfig::default()
        };

        match dispatch_uninstall_with_config(slug, &config).await {
            Ok(detail) => summary.push_str(&format!("{detail}\n\n")),
            Err(e) => {
                any_failed = true;
                summary.push_str(&format!("FAIL  {e}\n\n"));
                tracing::warn!(
                    component = %slug,
                    error = %e,
                    "rollback: dispatch_uninstall_with_config failed; continuing with the rest of the chain"
                );
            }
        }
        if let Some(store) = &store {
            // Drop the instance row.
            let kind = format!("{slug}-instance");
            let key = ResourceKey::cluster_scoped(&kind, "local");
            if let Err(e) = store.delete(&key, None).await {
                tracing::warn!(
                    error = %e,
                    component = %slug,
                    "rollback: store.delete({slug}-instance/local) failed; \
                     row may linger -- inspect via /resource and clean up manually"
                );
                summary.push_str(&format!(
                    "Note: failed to drop {slug}-instance/local from metadata store ({e}).\n\n"
                ));
            }
            // Drop the persisted install-config so the next install
            // of this slug doesn't inherit stale overrides.
            delete_install_config(store.as_ref(), slug).await;
        }
        // Drop the encrypted admin credential too -- the service is
        // gone, the credential is meaningless.
        if let Some(secrets) = &secrets {
            let secret_ref = format!("{slug}/admin-password");
            if let Err(e) = secrets.delete(&secret_ref).await {
                tracing::debug!(
                    error = %e,
                    component = %slug,
                    "rollback: secrets.delete({secret_ref}) failed; \
                     ignoring -- the entry may not have existed"
                );
            }
        }
    }

    Html(render_install_result(&l, !any_failed, &summary)).into_response()
}

/// GET /api/install/job/{id} -- JSON snapshot of progress, polled by
/// the wizard's JS every ~500ms.
async fn install_job_api_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Response {
    let snapshot = state
        .jobs
        .lock()
        .unwrap()
        .get(&job_id)
        .map(|s| s.lock().unwrap().clone());
    match snapshot {
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown job"})),
        )
            .into_response(),
        Some(p) => {
            let ratio = p.phase_ratio();
            // Augment the serialized progress with the precomputed
            // phase_ratio so the client doesn't need to know the
            // bytes-vs-other-phases rule.
            let mut value = serde_json::to_value(&p).unwrap_or_else(|_| serde_json::json!({}));
            if let serde_json::Value::Object(m) = &mut value {
                m.insert("phase_ratio".into(), serde_json::Value::from(ratio));
                m.insert(
                    "phase_label".into(),
                    serde_json::Value::from(p.phase.label()),
                );
            }
            Json(value).into_response()
        }
    }
}

/// Run the platform-specific Postgres install and return either
/// `(human_summary, listening_port)` on success or a failure detail on
/// error. The port is forwarded to `install_postgres_handler` so it can
/// register a `postgres-instance/local` resource in the state store
/// reflecting where the freshly-installed server is listening.
///
/// `_progress` is used on Windows where the install path streams a
/// large binary bundle and benefits from a live wizard. Linux + macOS
/// installs are fast enough that they don't need a progress bar yet
/// (the wizard will still show the final result page).
#[cfg(target_os = "linux")]
async fn run_postgres_install_with_progress(
    _progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::postgres;
    let mut opts = postgres::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.unit_name = format!("{s}.service");
    }
    match postgres::install(opts).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\ndata_dir: {}\nsystemd unit: {}\nport: {}\npsql symlink: {}",
                r.bin_dir.display(),
                r.data_dir.display(),
                r.unit_path.display(),
                r.port,
                r.psql_symlink
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not created)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "macos")]
async fn run_postgres_install_with_progress(
    _progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::macos::postgres;
    let mut opts = postgres::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.plist_name = format!("{s}.plist");
    }
    match postgres::install(opts).await {
        Ok(r) => Ok((
            format!(
                "bin_dir: {}\ndata_dir: {}\nlaunchd plist: {}\nport: {}\npsql symlink: {}",
                r.bin_dir.display(),
                r.data_dir.display(),
                r.plist_path.display(),
                r.port,
                r.psql_symlink
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not created)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "windows")]
async fn run_postgres_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::windows::postgres;
    let mut opts = postgres::InstallOptions::default();
    if let Some(p) = config.port {
        opts.port = p;
    }
    if let Some(d) = &config.root_dir {
        opts.root_dir = std::path::PathBuf::from(d);
    }
    if let Some(s) = &config.service_name {
        opts.service_name = s.clone();
    }
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match postgres::install_with_progress(opts, progress).await {
        Ok(r) => {
            let shim_line = match (&r.psql_shim, &r.psql_shim_error) {
                (Some(p), _) => p.display().to_string(),
                (None, Some(err)) => format!("(failed: {err})"),
                (None, None) => "(not created)".into(),
            };
            Ok((
                format!(
                    "bin_dir: {}\ndata_dir: {}\nservice: {}\nport: {}\npsql shim: {}",
                    r.bin_dir.display(),
                    r.data_dir.display(),
                    r.service_name,
                    r.port,
                    shim_line,
                ),
                r.port,
            ))
        }
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn run_postgres_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
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

async fn state_page_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let Some(store) = &state.store else {
        return Html(render_state_page(&l, None));
    };
    // Pairs with the `/api/state/info` JSON endpoint -- same kinds,
    // same per-kind list() calls. Any errors are surfaced inline in
    // the row rather than crashing the page.
    let kinds: &[(&str, &str)] = &[
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
    let mut rows: Vec<StateRow> = Vec::with_capacity(kinds.len());
    for (kind, slug) in kinds {
        let count = match store.list(kind, None).await {
            Ok(v) => Ok(v.len()),
            Err(e) => Err(e.to_string()),
        };
        rows.push(StateRow {
            kind: (*kind).into(),
            component_label: l.t(&format!("component-{slug}-name")),
            count,
        });
    }
    Html(render_state_page(&l, Some(&rows)))
}

/// One row in the `/state` table.
#[derive(Clone, Debug)]
pub struct StateRow {
    /// Resource kind, e.g. `postgres-instance`.
    pub kind: String,
    /// Localized component name, e.g. "PostgreSQL".
    pub component_label: String,
    /// Instance count, or the error string from the store if listing failed.
    pub count: Result<usize, String>,
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

async fn resource_handler(
    State(state): State<AppState>,
    Path((kind, name)): Path<(String, String)>,
) -> (StatusCode, Html<String>) {
    let l = Localizer::english();
    let Some(store) = &state.store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render_resource(&l, &kind, &name, None, None, true)),
        );
    };
    // If this is a managed component instance, eagerly load the
    // persisted InstallConfig so the repair button can submit the
    // operator's chosen service-name / root-dir / port / version as
    // hidden inputs instead of letting the driver fall back to
    // defaults.
    let install_config: Option<InstallConfig> = match kind.strip_suffix("-instance") {
        Some(slug) if INSTALL_ORDER.contains(&slug) => {
            load_install_config(store.as_ref(), slug).await
        }
        _ => None,
    };
    let key = ResourceKey::cluster_scoped(&kind, &name);
    match store.load(&key).await {
        Ok(Some(stored)) => (
            StatusCode::OK,
            Html(render_resource(
                &l,
                &kind,
                &name,
                Some(&stored),
                install_config.as_ref(),
                false,
            )),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Html(render_resource(&l, &kind, &name, None, None, false)),
        ),
        Err(e) => {
            tracing::warn!(
                error = %e,
                kind = %kind,
                name = %name,
                "resource_handler: store.load failed; surfacing not-found page. \
                 Likely causes: (1) SQLite file unreadable, (2) schema mismatch \
                 from a prior version. Check the server logs and the state.db path."
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(render_resource(&l, &kind, &name, None, None, false)),
            )
        }
    }
}

async fn resource_delete_handler(
    State(state): State<AppState>,
    Path((kind, name)): Path<(String, String)>,
) -> (StatusCode, Html<String>) {
    let l = Localizer::english();
    let Some(store) = &state.store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render_install_result(
                &l,
                false,
                &l.t("ui-resource-store-missing"),
            )),
        );
    };
    let key = ResourceKey::cluster_scoped(&kind, &name);
    match store.delete(&key, None).await {
        Ok(()) => (
            StatusCode::OK,
            Html(render_resource_deleted(&l, &kind, &name)),
        ),
        Err(e) => {
            tracing::warn!(
                error = %e,
                kind = %kind,
                name = %name,
                "resource_delete_handler: store.delete failed; likely cause: resource no longer exists. \
                 The page surfaces the error so the operator can investigate via /status."
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(render_install_result(
                    &l,
                    false,
                    &format!("{}: {e}", l.t("ui-resource-delete-failed")),
                )),
            )
        }
    }
}

async fn home_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let store_summary = match &state.store {
        None => StoreSummary::Missing,
        Some(store) => {
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
            let mut total: usize = 0;
            for k in kinds {
                if let Ok(v) = store.list(k, None).await {
                    total += v.len();
                }
            }
            StoreSummary::Counted(total)
        }
    };
    Html(render_home(&l, store_summary))
}

/// Top-level state-store summary shown in the home dashboard's
/// `Metadata store` card.
#[derive(Clone, Copy, Debug)]
pub enum StoreSummary {
    /// No store attached on this server (smoke router).
    Missing,
    /// Store attached; total resource count across all known kinds.
    Counted(usize),
}

/// Form submitted to `POST /login`.
#[derive(serde::Deserialize)]
struct LoginForm {
    /// Operator username.
    username: String,
    /// Plaintext password. Never logged.
    password: String,
    /// Optional `?next=<path>` redirect target carried through from
    /// the auth middleware. Validated server-side -- only relative
    /// paths starting with `/` are honored, so an attacker can't use
    /// the login redirect as an open-redirect into another origin.
    #[serde(default)]
    next: String,
}

/// GET /login -- render the sign-in form. When no operator account
/// exists yet, redirect to `/setup` instead so the operator gets the
/// first-boot flow without having to know the URL.
async fn login_form_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let l = Localizer::english();
    if let Some(ops) = &state.operators {
        if ops.is_empty().await {
            return Redirect303("/setup".into()).into_response();
        }
    }
    let next = params.get("next").cloned().unwrap_or_default();
    Html(render_login(&l, &next, None)).into_response()
}

/// POST /login -- verify credentials, mint a session, set the cookie,
/// redirect onward.
async fn login_post_handler(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Response {
    let l = Localizer::english();
    let Some(operators) = &state.operators else {
        // No operator file = no auth = nothing to log into.
        return Redirect303("/".into()).into_response();
    };

    match operators.verify(&form.username, &form.password).await {
        Ok(rec) => {
            let session_id = state.sessions.create(rec.username.clone()).await;
            // The session we just minted carries a freshly-generated
            // csrf_token; read it back so we can set both cookies in
            // the response.
            let csrf_token = state
                .sessions
                .get(&session_id)
                .await
                .map(|s| s.csrf_token)
                .unwrap_or_default();
            let session_cookie = auth::session_cookie_header(&session_id);
            let csrf_cookie = auth::csrf_cookie_header(&csrf_token);
            let target = safe_next_path(&form.next).unwrap_or_else(|| "/".to_string());
            let mut response = Redirect303(target).into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(&session_cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            if let Ok(value) = axum::http::HeaderValue::from_str(&csrf_cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            tracing::info!(
                username = %rec.username,
                "operator signed in; session minted and cookies set"
            );
            if let Some(audit) = &state.audit {
                let _ = audit
                    .append(
                        rec.username.clone(),
                        computeza_audit::Action::Authn,
                        None,
                        serde_json::json!({"username": rec.username}),
                    )
                    .await;
            }
            response
        }
        Err(_) => {
            if let Some(audit) = &state.audit {
                let _ = audit
                    .append(
                        form.username.clone(),
                        computeza_audit::Action::Authn,
                        None,
                        serde_json::json!({"username": form.username}),
                    )
                    .await;
            }
            let failure = l.t("ui-login-failed");
            Html(render_login(&l, &form.next, Some(&failure))).into_response()
        }
    }
}

/// Validate a `next` redirect target. Only relative paths starting
/// with a single `/` (and not `//`, which would be a protocol-relative
/// URL) are honored.
fn safe_next_path(next: &str) -> Option<String> {
    if next.is_empty() {
        return None;
    }
    if !next.starts_with('/') {
        return None;
    }
    if next.starts_with("//") {
        return None;
    }
    Some(next.to_string())
}

/// Form submitted to `POST /setup`.
#[derive(serde::Deserialize)]
struct SetupForm {
    username: String,
    password: String,
    password_confirm: String,
}

/// GET /setup -- render the first-boot operator creation form.
/// Refuses (with an explanatory page) once at least one operator
/// exists, so an attacker who lands on the URL after first boot
/// cannot create additional operators without authenticating.
async fn setup_form_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    if let Some(ops) = &state.operators {
        if !ops.is_empty().await {
            return Html(render_setup_already_done(&l)).into_response();
        }
    }
    Html(render_setup(&l, None)).into_response()
}

/// POST /setup -- create the first operator account, mint a session,
/// and sign them in.
async fn setup_post_handler(
    State(state): State<AppState>,
    Form(form): Form<SetupForm>,
) -> Response {
    let l = Localizer::english();
    let Some(operators) = &state.operators else {
        return Redirect303("/".into()).into_response();
    };
    if !operators.is_empty().await {
        return Html(render_setup_already_done(&l)).into_response();
    }
    if form.password != form.password_confirm {
        let msg = l.t("ui-setup-password-mismatch");
        return Html(render_setup(&l, Some(&msg))).into_response();
    }
    // First-boot operator is always an admin so they have somewhere
    // to operate from. Additional operators (and their group
    // memberships) are managed from /admin/operators once at least
    // one admin exists.
    if let Err(e) = operators
        .create(&form.username, &form.password, &["admins".to_string()])
        .await
    {
        return Html(render_setup(&l, Some(&e.to_string()))).into_response();
    }
    let session_id = state.sessions.create(form.username.clone()).await;
    let csrf_token = state
        .sessions
        .get(&session_id)
        .await
        .map(|s| s.csrf_token)
        .unwrap_or_default();
    let session_cookie = auth::session_cookie_header(&session_id);
    let csrf_cookie = auth::csrf_cookie_header(&csrf_token);
    let mut response = Redirect303("/".into()).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&session_cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&csrf_cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// POST /logout -- destroy the current session and clear the cookie.
async fn logout_handler(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if let Some(cookie_header) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        if let Some(id) = auth::session_id_from_cookies(cookie_header) {
            state.sessions.destroy(&id).await;
        }
    }
    let mut response = Redirect303("/".into()).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&auth::clear_session_cookie_header()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&auth::clear_csrf_cookie_header()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// GET /account -- operator account detail page with a Sign out
/// form. Auth-required: the middleware bounces unauthenticated
/// requests to /login first.
async fn account_handler(axum::Extension(session): axum::Extension<auth::Session>) -> Html<String> {
    let l = Localizer::english();
    Html(render_account(&l, &session))
}

/// GET /audit -- recent audit-log events. Auth-required (the
/// middleware bounces unauthenticated visitors to /login). Caps at
/// 200 rows; older events stay on disk but the viewer doesn't
/// paginate yet (v0.1+ refinement).
async fn audit_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let Some(audit) = &state.audit else {
        return Html(render_audit(&l, None, None));
    };
    let events = audit.list_recent(200).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "audit list_recent failed; rendering empty page");
        Vec::new()
    });
    let verifying_key = audit.verifying_key_b64().await;
    Html(render_audit(&l, Some(&events), Some(&verifying_key)))
}

/// GET /admin/operators -- list every operator account with their
/// group memberships, plus a "create new operator" form. Manage-only
/// (the permission middleware bounces non-admins with 403).
async fn admin_operators_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
) -> Html<String> {
    let l = Localizer::english();
    let operators = match &state.operators {
        Some(o) => o.list().await,
        None => Vec::new(),
    };
    Html(render_admin_operators(
        &l,
        &operators,
        &session.username,
        None,
    ))
}

#[derive(serde::Deserialize)]
struct CreateOperatorForm {
    username: String,
    password: String,
    /// Comma-separated group names. e.g. "operators,viewers". The
    /// handler trims + filters empties.
    #[serde(default)]
    groups: String,
}

/// POST /admin/operators -- create a new operator account.
async fn admin_create_operator_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Form(form): Form<CreateOperatorForm>,
) -> Response {
    let l = Localizer::english();
    let Some(operators) = &state.operators else {
        return Redirect303("/admin/operators".into()).into_response();
    };
    let groups: Vec<String> = form
        .groups
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let groups = if groups.is_empty() {
        vec!["operators".to_string()]
    } else {
        groups
    };
    if let Err(e) = operators
        .create(&form.username, &form.password, &groups)
        .await
    {
        let list = operators.list().await;
        return Html(render_admin_operators(
            &l,
            &list,
            &session.username,
            Some(&e.to_string()),
        ))
        .into_response();
    }
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("operator/{}", form.username)),
                serde_json::json!({
                    "action": "create_operator",
                    "username": form.username,
                    "groups": groups,
                }),
            )
            .await;
    }
    Redirect303("/admin/operators".into()).into_response()
}

#[derive(serde::Deserialize)]
struct UpdateGroupsForm {
    groups: String,
}

/// POST /admin/operators/{name}/groups -- replace an operator's
/// group memberships.
async fn admin_update_operator_groups_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Path(name): Path<String>,
    Form(form): Form<UpdateGroupsForm>,
) -> Response {
    let l = Localizer::english();
    let Some(operators) = &state.operators else {
        return Redirect303("/admin/operators".into()).into_response();
    };
    let groups: Vec<String> = form
        .groups
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if let Err(e) = operators.set_groups(&name, &groups).await {
        let list = operators.list().await;
        return Html(render_admin_operators(
            &l,
            &list,
            &session.username,
            Some(&format!("Updating {name}: {e}")),
        ))
        .into_response();
    }
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("operator/{name}")),
                serde_json::json!({
                    "action": "update_groups",
                    "username": name,
                    "groups": groups,
                }),
            )
            .await;
    }
    Redirect303("/admin/operators".into()).into_response()
}

/// POST /admin/operators/{name}/delete -- delete an operator
/// account. Refuses to delete the currently-signed-in admin or the
/// last remaining admin (the console would lose its management
/// surface otherwise).
async fn admin_delete_operator_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Path(name): Path<String>,
) -> Response {
    let l = Localizer::english();
    let Some(operators) = &state.operators else {
        return Redirect303("/admin/operators".into()).into_response();
    };
    if name == session.username {
        let list = operators.list().await;
        return Html(render_admin_operators(
            &l,
            &list,
            &session.username,
            Some(&l.t("ui-admin-operators-cant-delete-self")),
        ))
        .into_response();
    }
    // Last-admin protection: if this is an admins-group member and
    // it's the ONLY admins-group member, refuse.
    let list = operators.list().await;
    let target_is_admin = list
        .iter()
        .find(|r| r.username == name)
        .map(|r| r.groups.iter().any(|g| g == "admins"))
        .unwrap_or(false);
    let admin_count = list
        .iter()
        .filter(|r| r.groups.iter().any(|g| g == "admins"))
        .count();
    if target_is_admin && admin_count <= 1 {
        return Html(render_admin_operators(
            &l,
            &list,
            &session.username,
            Some(&l.t("ui-admin-operators-cant-delete-last-admin")),
        ))
        .into_response();
    }
    if let Err(e) = operators.delete(&name).await {
        let list = operators.list().await;
        return Html(render_admin_operators(
            &l,
            &list,
            &session.username,
            Some(&format!("Deleting {name}: {e}")),
        ))
        .into_response();
    }
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("operator/{name}")),
                serde_json::json!({"action": "delete_operator", "username": name}),
            )
            .await;
    }
    Redirect303("/admin/operators".into()).into_response()
}

/// GET /admin/groups -- read-only listing of built-in groups + the
/// permissions each grants. v0.0.x ships three built-in groups;
/// custom groups land in v0.1+.
async fn admin_groups_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_admin_groups(&l))
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

async fn logo_handler() -> Response {
    serve_svg(COMPUTEZA_LOGO_SVG)
}

async fn logo_mono_light_handler() -> Response {
    serve_svg(COMPUTEZA_LOGO_MONO_LIGHT_SVG)
}

async fn favicon_handler() -> Response {
    serve_svg(COMPUTEZA_FAVICON_SVG)
}

async fn lottie_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        COMPUTEZA_LOTTIE_PULSE,
    )
        .into_response()
}

fn serve_svg(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

/// Identifies which top-nav link should be highlighted for the
/// currently rendered page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavLink {
    /// Home dashboard.
    Home,
    /// Managed components reference.
    Components,
    /// Install wizard.
    Install,
    /// Live reconciler status dashboard.
    Status,
    /// Metadata store summary.
    State,
    /// Admin / secrets page.
    Secrets,
    /// Operator account / sign-out page.
    Account,
    /// Audit log viewer.
    Audit,
    /// Admin operator-management page.
    Operators,
    /// Admin group-listing page.
    Groups,
    /// No nav link highlighted (e.g. resource detail page).
    None,
}

/// Render the shared shell: doctype, head, top nav bar with the brand
/// mark, the centered page container, and the footer. `body` is HTML
/// dropped verbatim inside the page container. `active` highlights
/// one of the top-nav links.
#[must_use]
pub fn render_shell(
    localizer: &Localizer,
    page_title: &str,
    active: NavLink,
    body: &str,
) -> String {
    let app_title = localizer.t("ui-app-title");
    let nav_components = localizer.t("ui-nav-components");
    let nav_install = localizer.t("ui-nav-install");
    let nav_status = localizer.t("ui-nav-status");
    let nav_state = localizer.t("ui-nav-state");
    let nav_secrets = localizer.t("ui-admin-secrets");
    let nav_account = localizer.t("ui-nav-account");
    let nav_audit = localizer.t("ui-audit-nav");
    let nav_admin_operators = localizer.t("ui-nav-admin-operators");
    let nav_admin_groups = localizer.t("ui-nav-admin-groups");
    let version_label = localizer.t("ui-footer-version");
    let version = env!("CARGO_PKG_VERSION");

    let nav_class = |link: NavLink| -> &'static str {
        if active == link {
            "cz-active"
        } else {
            ""
        }
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{page_title} -- {app_title}</title>
<link rel="icon" type="image/svg+xml" href="/static/brand/computeza-favicon.svg" />
<link rel="stylesheet" href="/static/computeza.css" />
</head>
<body>
<nav class="cz-nav">
  <a href="/" class="cz-brand">
    <img src="/static/brand/computeza-logo.svg" alt="" />
    <span>{app_title}</span>
  </a>
  <div class="cz-navlinks">
    <a href="/components" class="{nc}">{nav_components}</a>
    <a href="/install" class="{ni}">{nav_install}</a>
    <a href="/status" class="{ns}">{nav_status}</a>
    <a href="/state" class="{nm}">{nav_state}</a>
    <a href="/admin/secrets" class="{na}">{nav_secrets}</a>
    <a href="/audit" class="{naud}">{nav_audit}</a>
    <a href="/admin/operators" class="{nops}">{nav_admin_operators}</a>
    <a href="/admin/groups" class="{ngrp}">{nav_admin_groups}</a>
    <a href="/account" class="{nacc}">{nav_account}</a>
  </div>
</nav>
<main class="cz-page">
{body}
<footer class="cz-footer">
  <span>{version_label} {version}</span>
  <span>Computeza proprietary -- managed components retain upstream licenses (see /components)</span>
</footer>
</main>
<script>
// CSRF token auto-fill. The auth middleware sets a non-HttpOnly
// computeza_csrf cookie on every authenticated request; on every
// form submit we read it and populate any empty csrf_token inputs
// before the form actually submits. Forms on public pages (login /
// setup) have no csrf_token input, so this runs as a no-op there.
(function () {{
  function readCsrf() {{
    const parts = document.cookie.split(";");
    for (const p of parts) {{
      const t = p.trim();
      if (t.startsWith("computeza_csrf=")) return t.substring(15);
    }}
    return "";
  }}
  document.addEventListener("submit", function (e) {{
    const token = readCsrf();
    if (!token) return;
    const inputs = e.target.querySelectorAll('input[name="csrf_token"]');
    inputs.forEach(function (i) {{ if (!i.value) i.value = token; }});
  }}, true);
}})();
</script>
</body>
</html>"#,
        nc = nav_class(NavLink::Components),
        ni = nav_class(NavLink::Install),
        ns = nav_class(NavLink::Status),
        nm = nav_class(NavLink::State),
        na = nav_class(NavLink::Secrets),
        naud = nav_class(NavLink::Audit),
        nops = nav_class(NavLink::Operators),
        ngrp = nav_class(NavLink::Groups),
        nacc = nav_class(NavLink::Account),
    )
}

/// Render the home page to a complete HTML document.
///
/// `store_summary` drives the "Metadata store" card -- pass
/// `StoreSummary::Missing` when no store is attached to this server.
#[must_use]
pub fn render_home(localizer: &Localizer, _store_summary: StoreSummary) -> String {
    render_landing_page(localizer)
}

/// Render the public marketing landing page at `/`. Unauthenticated
/// surface: anyone visiting the operator console root sees this page
/// before being asked to sign in. Mirrors the multi-section structure
/// of mature B2B SaaS landings (hero + stats + about + features +
/// audiences + pricing + final CTA) so a CISO browsing the marketing
/// copy gets the full operator pitch without needing to drop into
/// the console first.
///
/// All visible strings flow through the i18n bundle so the reseller
/// chain (v0.1+) can swap brand voice per tenant.
#[must_use]
pub fn render_landing_page(localizer: &Localizer) -> String {
    let title = localizer.t("ui-welcome-title");

    // Hero
    let hero_eyebrow = localizer.t("ui-landing-hero-eyebrow");
    let hero_title_pre = localizer.t("ui-landing-hero-title-pre");
    let hero_title_em = localizer.t("ui-landing-hero-title-em");
    let hero_subtitle = localizer.t("ui-landing-hero-subtitle");
    let hero_cta_primary = localizer.t("ui-landing-hero-cta-primary");
    let hero_cta_secondary = localizer.t("ui-landing-hero-cta-secondary");

    // Stats
    let stat_1_value = localizer.t("ui-landing-stat-1-value");
    let stat_1_label = localizer.t("ui-landing-stat-1-label");
    let stat_2_value = localizer.t("ui-landing-stat-2-value");
    let stat_2_label = localizer.t("ui-landing-stat-2-label");
    let stat_3_value = localizer.t("ui-landing-stat-3-value");
    let stat_3_label = localizer.t("ui-landing-stat-3-label");
    let stat_4_value = localizer.t("ui-landing-stat-4-value");
    let stat_4_label = localizer.t("ui-landing-stat-4-label");

    // About
    let about_eyebrow = localizer.t("ui-landing-about-eyebrow");
    let about_title = localizer.t("ui-landing-about-title");
    let about_subtitle = localizer.t("ui-landing-about-subtitle");

    // Features
    let features_eyebrow = localizer.t("ui-landing-features-eyebrow");
    let features_title = localizer.t("ui-landing-features-title");
    let features_subtitle = localizer.t("ui-landing-features-subtitle");

    let features = [
        (
            "01",
            localizer.t("ui-landing-feature-1-title"),
            localizer.t("ui-landing-feature-1-body"),
        ),
        (
            "02",
            localizer.t("ui-landing-feature-2-title"),
            localizer.t("ui-landing-feature-2-body"),
        ),
        (
            "03",
            localizer.t("ui-landing-feature-3-title"),
            localizer.t("ui-landing-feature-3-body"),
        ),
        (
            "04",
            localizer.t("ui-landing-feature-4-title"),
            localizer.t("ui-landing-feature-4-body"),
        ),
        (
            "05",
            localizer.t("ui-landing-feature-5-title"),
            localizer.t("ui-landing-feature-5-body"),
        ),
        (
            "06",
            localizer.t("ui-landing-feature-6-title"),
            localizer.t("ui-landing-feature-6-body"),
        ),
        (
            "07",
            localizer.t("ui-landing-feature-7-title"),
            localizer.t("ui-landing-feature-7-body"),
        ),
        (
            "08",
            localizer.t("ui-landing-feature-8-title"),
            localizer.t("ui-landing-feature-8-body"),
        ),
        (
            "09",
            localizer.t("ui-landing-feature-9-title"),
            localizer.t("ui-landing-feature-9-body"),
        ),
    ];
    let feature_cards: String = features
        .iter()
        .map(|(icon, t, b)| {
            format!(
                r#"<div class="cz-feature">
<span class="cz-feature-icon">{icon}</span>
<h3 class="cz-feature-title">{t}</h3>
<p class="cz-feature-body">{b}</p>
</div>"#,
                icon = html_escape(icon),
                t = html_escape(t),
                b = html_escape(b),
            )
        })
        .collect();

    // Audiences
    let audiences_eyebrow = localizer.t("ui-landing-audiences-eyebrow");
    let audiences_title = localizer.t("ui-landing-audiences-title");
    let audiences_subtitle = localizer.t("ui-landing-audiences-subtitle");
    let personas = [
        (
            localizer.t("ui-landing-audience-1-role"),
            localizer.t("ui-landing-audience-1-title"),
            localizer.t("ui-landing-audience-1-body"),
        ),
        (
            localizer.t("ui-landing-audience-2-role"),
            localizer.t("ui-landing-audience-2-title"),
            localizer.t("ui-landing-audience-2-body"),
        ),
        (
            localizer.t("ui-landing-audience-3-role"),
            localizer.t("ui-landing-audience-3-title"),
            localizer.t("ui-landing-audience-3-body"),
        ),
    ];
    let persona_cards: String = personas
        .iter()
        .map(|(role, t, b)| {
            format!(
                r#"<div class="cz-persona">
<p class="cz-persona-role">{role}</p>
<h3 class="cz-persona-title">{t}</h3>
<p class="cz-persona-body">{b}</p>
</div>"#,
                role = html_escape(role),
                t = html_escape(t),
                b = html_escape(b),
            )
        })
        .collect();

    // Pricing
    let pricing_eyebrow = localizer.t("ui-landing-pricing-eyebrow");
    let pricing_title = localizer.t("ui-landing-pricing-title");
    let pricing_subtitle = localizer.t("ui-landing-pricing-subtitle");

    let tier1 = render_pricing_card(
        &localizer.t("ui-landing-pricing-1-name"),
        &localizer.t("ui-landing-pricing-1-price"),
        &localizer.t("ui-landing-pricing-1-unit"),
        &localizer.t("ui-landing-pricing-1-tagline"),
        &[
            localizer.t("ui-landing-pricing-1-feature-1"),
            localizer.t("ui-landing-pricing-1-feature-2"),
            localizer.t("ui-landing-pricing-1-feature-3"),
            localizer.t("ui-landing-pricing-1-feature-4"),
            localizer.t("ui-landing-pricing-1-feature-5"),
        ],
        &localizer.t("ui-landing-pricing-1-cta"),
        "/install",
        None,
        false,
    );
    let tier2 = render_pricing_card(
        &localizer.t("ui-landing-pricing-2-name"),
        &localizer.t("ui-landing-pricing-2-price"),
        &localizer.t("ui-landing-pricing-2-unit"),
        &localizer.t("ui-landing-pricing-2-tagline"),
        &[
            localizer.t("ui-landing-pricing-2-feature-1"),
            localizer.t("ui-landing-pricing-2-feature-2"),
            localizer.t("ui-landing-pricing-2-feature-3"),
            localizer.t("ui-landing-pricing-2-feature-4"),
            localizer.t("ui-landing-pricing-2-feature-5"),
        ],
        &localizer.t("ui-landing-pricing-2-cta"),
        "/login",
        Some(&localizer.t("ui-landing-pricing-2-badge")),
        true,
    );
    let tier3 = render_pricing_card(
        &localizer.t("ui-landing-pricing-3-name"),
        &localizer.t("ui-landing-pricing-3-price"),
        &localizer.t("ui-landing-pricing-3-unit"),
        &localizer.t("ui-landing-pricing-3-tagline"),
        &[
            localizer.t("ui-landing-pricing-3-feature-1"),
            localizer.t("ui-landing-pricing-3-feature-2"),
            localizer.t("ui-landing-pricing-3-feature-3"),
            localizer.t("ui-landing-pricing-3-feature-4"),
            localizer.t("ui-landing-pricing-3-feature-5"),
            localizer.t("ui-landing-pricing-3-feature-6"),
        ],
        &localizer.t("ui-landing-pricing-3-cta"),
        "/login",
        None,
        false,
    );

    // Final CTA
    let final_title = localizer.t("ui-landing-final-title");
    let final_subtitle = localizer.t("ui-landing-final-subtitle");
    let final_primary = localizer.t("ui-landing-final-primary");
    let final_secondary = localizer.t("ui-landing-final-secondary");

    let body = format!(
        r#"<div class="cz-landing">
<section class="cz-landing-hero">
<span class="cz-landing-eyebrow">{hero_eyebrow}</span>
<h1 class="cz-landing-title">{hero_title_pre}<br /><em>{hero_title_em}</em></h1>
<p class="cz-landing-subtitle">{hero_subtitle}</p>
<div class="cz-cta-row">
<a class="cz-btn cz-btn-primary cz-btn-lg" href="/login">{hero_cta_primary}</a>
<a class="cz-btn cz-btn-lg" href="/components">{hero_cta_secondary}</a>
</div>
<div class="cz-stat-strip">
<div class="cz-stat"><span class="cz-stat-value">{s1v}</span><span class="cz-stat-label">{s1l}</span></div>
<div class="cz-stat"><span class="cz-stat-value">{s2v}</span><span class="cz-stat-label">{s2l}</span></div>
<div class="cz-stat"><span class="cz-stat-value">{s3v}</span><span class="cz-stat-label">{s3l}</span></div>
<div class="cz-stat"><span class="cz-stat-value">{s4v}</span><span class="cz-stat-label">{s4l}</span></div>
</div>
</section>

<section class="cz-landing-section">
<div class="cz-landing-section-head">
<p class="cz-landing-section-eyebrow">{about_eyebrow}</p>
<h2 class="cz-landing-section-title">{about_title}</h2>
<p class="cz-landing-section-subtitle">{about_subtitle}</p>
</div>
</section>

<section class="cz-landing-section">
<div class="cz-landing-section-head">
<p class="cz-landing-section-eyebrow">{features_eyebrow}</p>
<h2 class="cz-landing-section-title">{features_title}</h2>
<p class="cz-landing-section-subtitle">{features_subtitle}</p>
</div>
<div class="cz-feature-grid">{feature_cards}</div>
</section>

<section class="cz-landing-section">
<div class="cz-landing-section-head">
<p class="cz-landing-section-eyebrow">{audiences_eyebrow}</p>
<h2 class="cz-landing-section-title">{audiences_title}</h2>
<p class="cz-landing-section-subtitle">{audiences_subtitle}</p>
</div>
<div class="cz-persona-grid">{persona_cards}</div>
</section>

<section class="cz-landing-section">
<div class="cz-landing-section-head">
<p class="cz-landing-section-eyebrow">{pricing_eyebrow}</p>
<h2 class="cz-landing-section-title">{pricing_title}</h2>
<p class="cz-landing-section-subtitle">{pricing_subtitle}</p>
</div>
<div class="cz-pricing-grid">
{tier1}
{tier2}
{tier3}
</div>
</section>

<section class="cz-landing-section">
<div class="cz-final-cta">
<h2>{final_title}</h2>
<p>{final_subtitle}</p>
<div class="cz-cta-row">
<a class="cz-btn cz-btn-primary cz-btn-lg" href="/login">{final_primary}</a>
<a class="cz-btn cz-btn-lg" href="/components">{final_secondary}</a>
</div>
</div>
</section>
</div>"#,
        hero_eyebrow = html_escape(&hero_eyebrow),
        hero_title_pre = html_escape(&hero_title_pre),
        hero_title_em = html_escape(&hero_title_em),
        hero_subtitle = html_escape(&hero_subtitle),
        hero_cta_primary = html_escape(&hero_cta_primary),
        hero_cta_secondary = html_escape(&hero_cta_secondary),
        s1v = html_escape(&stat_1_value),
        s1l = html_escape(&stat_1_label),
        s2v = html_escape(&stat_2_value),
        s2l = html_escape(&stat_2_label),
        s3v = html_escape(&stat_3_value),
        s3l = html_escape(&stat_3_label),
        s4v = html_escape(&stat_4_value),
        s4l = html_escape(&stat_4_label),
        about_eyebrow = html_escape(&about_eyebrow),
        about_title = html_escape(&about_title),
        about_subtitle = html_escape(&about_subtitle),
        features_eyebrow = html_escape(&features_eyebrow),
        features_title = html_escape(&features_title),
        features_subtitle = html_escape(&features_subtitle),
        feature_cards = feature_cards,
        audiences_eyebrow = html_escape(&audiences_eyebrow),
        audiences_title = html_escape(&audiences_title),
        audiences_subtitle = html_escape(&audiences_subtitle),
        persona_cards = persona_cards,
        pricing_eyebrow = html_escape(&pricing_eyebrow),
        pricing_title = html_escape(&pricing_title),
        pricing_subtitle = html_escape(&pricing_subtitle),
        tier1 = tier1,
        tier2 = tier2,
        tier3 = tier3,
        final_title = html_escape(&final_title),
        final_subtitle = html_escape(&final_subtitle),
        final_primary = html_escape(&final_primary),
        final_secondary = html_escape(&final_secondary),
    );

    render_shell(localizer, &title, NavLink::Home, &body)
}

/// Render a single pricing-tier card on the landing page.
/// `featured = true` swaps in the lavender-bordered variant with the
/// `data-badge` ribbon ("Most popular" or the localized equivalent).
#[allow(clippy::too_many_arguments)]
fn render_pricing_card(
    name: &str,
    price: &str,
    unit: &str,
    tagline: &str,
    features: &[String],
    cta_label: &str,
    cta_href: &str,
    badge: Option<&str>,
    featured: bool,
) -> String {
    let features_html: String = features
        .iter()
        .map(|f| format!("<li>{}</li>", html_escape(f)))
        .collect();
    let class = if featured {
        "cz-pricing cz-pricing-featured"
    } else {
        "cz-pricing"
    };
    let badge_attr = badge
        .map(|b| format!(r#" data-badge="{}""#, html_escape(b)))
        .unwrap_or_default();
    let btn_class = if featured {
        "cz-btn cz-btn-primary"
    } else {
        "cz-btn"
    };
    format!(
        r#"<div class="{class}"{badge_attr}>
<p class="cz-pricing-name">{name}</p>
<p class="cz-pricing-price">{price} <span class="cz-pricing-price-unit">{unit}</span></p>
<p class="cz-pricing-tagline">{tagline}</p>
<ul class="cz-pricing-features">{features_html}</ul>
<a class="{btn_class}" href="{cta_href}">{cta_label}</a>
</div>"#,
        class = class,
        badge_attr = badge_attr,
        name = html_escape(name),
        price = html_escape(price),
        unit = html_escape(unit),
        tagline = html_escape(tagline),
        features_html = features_html,
        btn_class = btn_class,
        cta_href = html_escape(cta_href),
        cta_label = html_escape(cta_label),
    )
}

/// Render the `/components` page: a table of every component the
/// platform manages, sourced from spec section 2.2 + per-component i18n
/// keys. Static for v0.0.x; future versions will surface live
/// reconciler status (drift indicators per spec section 4.4).
#[must_use]
pub fn render_components(localizer: &Localizer) -> String {
    let title = localizer.t("ui-components-title");
    let intro = localizer.t("ui-components-intro");
    let col_name = localizer.t("ui-components-col-name");
    let col_kind = localizer.t("ui-components-col-kind");
    let col_role = localizer.t("ui-components-col-role");
    let col_license = localizer.t("ui-components-col-license");
    let license_intro = localizer.t("ui-components-license-intro");

    // Per-component license + risk classification. The risk class
    // drives badge colour: permissive = ok (green), weak copyleft =
    // info (lavender), restrictive (BSL / Elastic v2) = warn (peach),
    // strong copyleft AGPL = fail (coral). Source of truth: docs/sbom.md
    // and docs/licensing.md.
    let components: &[(&str, &str, &str, &str)] = &[
        // (slug, kind, license, risk_class)
        ("kanidm", "identity", "MPL-2.0", "info"),
        ("garage", "object-storage", "AGPL-3.0", "fail"),
        ("lakekeeper", "catalog", "Apache-2.0", "ok"),
        ("xtable", "format-translation", "Apache-2.0", "ok"),
        ("databend", "sql-engine", "Elastic-2.0 / Apache-2.0", "warn"),
        ("qdrant", "vector", "Apache-2.0", "ok"),
        ("restate", "workflows", "BSL-1.1", "warn"),
        ("greptime", "observability", "Apache-2.0", "ok"),
        ("grafana", "dashboards", "AGPL-3.0", "fail"),
        ("postgres", "metadata-rdbms", "PostgreSQL License", "ok"),
        ("openfga", "authorization", "Apache-2.0", "ok"),
    ];

    let rows: String = components
        .iter()
        .map(|(slug, kind, license, risk)| {
            let name = localizer.t(&format!("component-{slug}-name"));
            let role = localizer.t(&format!("component-{slug}-role"));
            format!(
                "<tr><td class=\"cz-strong\">{name}</td>\
                 <td><span class=\"cz-badge cz-badge-info\">{kind}</span></td>\
                 <td class=\"cz-cell-dim\">{role}</td>\
                 <td><span class=\"cz-badge cz-badge-{risk}\">{license}</span></td></tr>",
                name = html_escape(&name),
                kind = html_escape(kind),
                role = html_escape(&role),
                risk = risk,
                license = html_escape(license),
            )
        })
        .collect();

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-table-wrap">
<table class="cz-table">
<thead><tr>
<th>{col_name}</th>
<th>{col_kind}</th>
<th>{col_role}</th>
<th>{col_license}</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{license_intro}</p>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        col_name = html_escape(&col_name),
        col_kind = html_escape(&col_kind),
        col_role = html_escape(&col_role),
        col_license = html_escape(&col_license),
        license_intro = html_escape(&license_intro),
    );

    render_shell(localizer, &title, NavLink::Components, &body)
}

/// Minimal percent-encoder for a single URL path segment. Encodes
/// everything except unreserved chars + `-._~`. Not a full RFC 3986
/// implementation; built for resource kinds and names, which are
/// kebab-case ASCII per the workspace conventions.
fn urlencoding_min(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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

/// Render the `/install` page: a wizard form that POSTs to
/// `/install/postgres` and lays down a native PostgreSQL service on the
/// host. Per spec section 2.1 / 4.2 the install path is GUI-equivalent
/// One row in the install hub: a component the operator can install.
#[derive(Clone, Copy, Debug)]
struct ComponentEntry {
    /// URL slug used by `/install/<slug>` -- matches the
    /// `<component>-instance` kind in the metadata store.
    slug: &'static str,
    /// i18n key for the localized display name (defined in `ui.ftl`
    /// under `component-<slug>-name`).
    name_key: &'static str,
    /// i18n key for the localized one-line role description.
    role_key: &'static str,
    /// When true, the install wizard is fully wired. When false the
    /// row links to a "coming soon" page.
    available: bool,
}

const COMPONENTS: &[ComponentEntry] = &[
    ComponentEntry {
        slug: "postgres",
        name_key: "component-postgres-name",
        role_key: "component-postgres-role",
        // Linux install live: downloads the PostgreSQL bundle from
        // EDB's CDN, runs initdb -U postgres, prepends pg_hba.conf
        // trust block, registers a systemd unit, starts. The wizard
        // refuses installs on non-Linux hosts via guard_supported_os.
        // The macOS + Windows postgres driver modules under
        // crates/computeza-driver-native/src/{macos,windows}/postgres.rs
        // exist as reference code but are not reachable through the
        // wizard for v0.0.x (Linux-only per the platform constraint).
        available: true,
    },
    ComponentEntry {
        slug: "kanidm",
        name_key: "component-kanidm-name",
        role_key: "component-kanidm-role",
        // Linux install path lives. Compiles kanidmd from crates.io
        // via `cargo install --locked --version <pin> --root <path>`,
        // generates a self-signed TLS cert via openssl, writes
        // server.toml, registers a systemd unit, starts. Operator
        // still needs `kanidmd recover_account admin` post-install
        // to bootstrap the admin password (surfaced in the wizard
        // result page).
        available: true,
    },
    ComponentEntry {
        slug: "garage",
        name_key: "component-garage-name",
        role_key: "component-garage-role",
        // Linux install live: downloads the raw binary from the
        // deuxfleurs CDN, writes garage.toml with dynamic ports
        // (s3=port, rpc=port+1, web=port+2, admin=port+3), registers
        // a systemd unit. Reconciler observes via the admin port.
        // No Windows / macOS driver -- Linux only for v0.0.x.
        available: true,
    },
    ComponentEntry {
        slug: "lakekeeper",
        name_key: "component-lakekeeper-name",
        role_key: "component-lakekeeper-role",
        // Linux install live: downloads the lakekeeper binary from the
        // GitHub release tar.gz, registers a systemd unit running
        // `lakekeeper serve` on the chosen REST port. Operator must
        // install postgres-instance first and supply the connection
        // string via the systemd unit's environment block before the
        // service becomes ready. Linux only for v0.0.x.
        available: true,
    },
    ComponentEntry {
        slug: "xtable",
        name_key: "component-xtable-name",
        role_key: "component-xtable-role",
        available: false,
    },
    ComponentEntry {
        slug: "databend",
        name_key: "component-databend-name",
        role_key: "component-databend-role",
        // Linux install live: downloads the databend-query binary from
        // the databendlabs GitHub release tarball, writes a minimal
        // databend-query.toml (fs storage backend) under root_dir/data,
        // registers a systemd unit. Linux only for v0.0.x. License is
        // restrictive (Elastic License 2.0 family) -- not a community
        // OSS license; flagged in docs/sbom.md.
        available: true,
    },
    ComponentEntry {
        slug: "qdrant",
        name_key: "component-qdrant-name",
        role_key: "component-qdrant-role",
        // Linux install live: downloads the qdrant binary from the
        // GitHub release tarball, writes config.yaml with the chosen
        // HTTP port + gRPC=port+1 + storage_path under data/,
        // registers a systemd unit. Linux only for v0.0.x.
        available: true,
    },
    ComponentEntry {
        slug: "restate",
        name_key: "component-restate-name",
        role_key: "component-restate-role",
        // Linux install live: downloads the `restate-server` binary
        // from the GitHub .tar.xz release (liblzma is statically
        // linked, so virgin Linux hosts without xz-utils still work),
        // registers a systemd unit. The CLI tools (restate +
        // restatectl) ship in separate tarballs upstream and aren't
        // bundled by the driver. Linux only for v0.0.x. License is
        // BSL 1.1 -- restrictive; flagged in docs/sbom.md.
        available: true,
    },
    ComponentEntry {
        slug: "greptime",
        name_key: "component-greptime-name",
        role_key: "component-greptime-role",
        // Linux install live: downloads the greptime binary tarball
        // from the GitHub release, registers a systemd unit running
        // `greptime standalone start` with HTTP bound on the chosen
        // port and the data home under the configured root_dir. Linux
        // only for v0.0.x.
        available: true,
    },
    ComponentEntry {
        slug: "grafana",
        name_key: "component-grafana-name",
        role_key: "component-grafana-role",
        // Linux install live: downloads the grafana binary tarball from
        // dl.grafana.com (Grafana ships no GitHub release assets),
        // registers a systemd unit running `grafana server --homepath
        // <root>/home` on the chosen port. Linux only for v0.0.x.
        // License is AGPL-3.0 -- process-isolated; see docs/licensing.md
        // for the section-5 aggregation argument.
        available: true,
    },
    ComponentEntry {
        slug: "openfga",
        name_key: "component-openfga-name",
        role_key: "component-openfga-role",
        // Linux install live: downloads the openfga binary from the
        // GitHub release tarball, registers a systemd unit running
        // `openfga run --datastore-engine memory` on the chosen port
        // (HTTP) + port+1 (gRPC). Reconciler observes via HTTP.
        // No Windows / macOS driver -- Linux only for v0.0.x.
        available: true,
    },
];

/// Canonical install order. Components are laid down sequentially in
/// this sequence so that downstream consumers find their dependencies
/// already up: lakekeeper opens a Postgres connection at start, every
/// component eventually federates auth through kanidm, etc. The order
/// preserves the postgres-first rule from the playbook (see AGENTS.md
/// "Component installer playbook") and groups stateful storage
/// (garage, qdrant) ahead of the query / observability layers.
///
/// Slugs in this list MUST also appear as `available: true` entries in
/// [`COMPONENTS`]; unavailable entries (today: xtable) are skipped at
/// dispatch time.
const INSTALL_ORDER: &[&str] = &[
    "postgres",
    "openfga",
    "kanidm",
    "garage",
    "qdrant",
    "lakekeeper",
    "greptime",
    "grafana",
    "restate",
    "databend",
];

/// Per-component canonical defaults shown as `placeholder=` text in the
/// unified install form. The driver itself owns the actual defaults --
/// these are surfaced purely so the operator sees "what would happen
/// if I leave this blank" without having to read driver source.
///
/// Returns `(default_port, default_service_name)`. Port 0 indicates
/// the component has no canonical port (xtable today).
fn canonical_defaults_for(slug: &str) -> (u16, String) {
    let port: u16 = match slug {
        "postgres" => 5432,
        "kanidm" => 8443,
        "garage" => 3900,
        "openfga" => 8080,
        "qdrant" => 6333,
        "greptime" => 4000,
        "lakekeeper" => 8181,
        "databend" => 8000,
        "grafana" => 3000,
        "restate" => 8081,
        _ => 0,
    };
    (port, format!("computeza-{slug}"))
}

/// Render one component as a stackable accordion row inside the unified
/// install form. Collapsed by default; the summary shows the
/// component name, its one-line role description, and a status badge.
/// Opening the row reveals the service-config inputs (service name,
/// port, data dir, version) plus a v0.1+ placeholder block for the
/// "Identity and access" surface (service account, credentials,
/// permissions, upstream IdP federation).
///
/// A small inline JS hook flips a `Reviewed` badge on once the
/// operator has expanded and then collapsed the row, so a vertical
/// scan of the install page shows at-a-glance which components have
/// been touched and which still need attention.
///
/// Inputs use the `<slug>__<field>` naming convention so the unified
/// POST handler can extract per-slug configs from a flat HashMap.
/// Blank fields fall through to driver defaults.
fn render_unified_component_card(localizer: &Localizer, c: &ComponentEntry) -> String {
    let name = localizer.t(c.name_key);
    let role = localizer.t(c.role_key);
    let identity_label = localizer.t("ui-install-card-identity");
    let identity_help = localizer.t("ui-install-card-identity-help");
    let identity_v01 = localizer.t("ui-install-card-identity-v01");
    let port_label = localizer.t("ui-install-port-label");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let version_label = localizer.t("ui-install-version-label");
    let unavailable_msg = localizer.t("ui-install-component-unavailable");
    let status_available = localizer.t("ui-install-status-available");
    let status_planned = localizer.t("ui-install-status-planned");
    let configured_label = localizer.t("ui-install-card-configured");

    let slug = c.slug;
    let (default_port, default_service) = canonical_defaults_for(slug);

    let (badge_class, badge_text) = if c.available {
        ("cz-badge cz-badge-info", status_available.as_str())
    } else {
        ("cz-badge cz-badge-info", status_planned.as_str())
    };

    let disabled_attr = if c.available { "" } else { " disabled" };

    let unavailable_block = if c.available {
        String::new()
    } else {
        format!(
            r#"<p class="cz-muted" style="margin: 0 0 0.75rem; font-size: 0.85rem;"><em>{}</em></p>"#,
            html_escape(&unavailable_msg)
        )
    };

    format!(
        r#"<li class="cz-install-row" id="card-{slug}" style="margin-bottom: 0.5rem;">
<details class="cz-install-details" style="background: rgba(255,255,255,0.04); border-radius: 0.6rem; overflow: hidden;">
<summary class="cz-install-summary" style="cursor: pointer; padding: 0.85rem 1rem; display: flex; align-items: center; justify-content: space-between; gap: 1rem; list-style: none;">
<div style="display: flex; flex-direction: column; gap: 0.2rem; flex: 1; min-width: 0;">
<span style="font-weight: 600;">{name}</span>
<span class="cz-muted" style="font-size: 0.82rem;">{role}</span>
</div>
<div style="display: flex; align-items: center; gap: 0.5rem; flex-shrink: 0;">
<span class="{badge_class}">{badge_text}</span>
<span class="cz-badge cz-badge-ok cz-install-configured-badge" style="display: none;">{configured_label}</span>
</div>
</summary>
<div style="padding: 0.25rem 1rem 1rem;">
{unavailable_block}
<div style="display: flex; flex-direction: column; gap: 0.55rem;">
<label for="{slug}__service_name" style="font-size: 0.85rem;">{service_name_label}</label>
<input id="{slug}__service_name" name="{slug}__service_name" class="cz-input" type="text" placeholder="{default_service}" pattern="[A-Za-z0-9_-]+"{disabled_attr} />
<label for="{slug}__port" style="font-size: 0.85rem;">{port_label}</label>
<input id="{slug}__port" name="{slug}__port" class="cz-input" type="number" min="1" max="65535" placeholder="{default_port}"{disabled_attr} />
<label for="{slug}__root_dir" style="font-size: 0.85rem;">{data_dir_label}</label>
<input id="{slug}__root_dir" name="{slug}__root_dir" class="cz-input" type="text" placeholder="/var/lib/computeza/{slug}"{disabled_attr} />
<label for="{slug}__version" style="font-size: 0.85rem;">{version_label}</label>
<input id="{slug}__version" name="{slug}__version" class="cz-input" type="text" placeholder="latest"{disabled_attr} />
</div>
<div style="margin-top: 0.85rem; padding: 0.75rem 0.9rem; border-radius: 0.5rem; background: rgba(245, 181, 68, 0.08); border: 1px solid rgba(245, 181, 68, 0.25);">
<p style="margin: 0 0 0.35rem; font-weight: 600; font-size: 0.85rem;">{identity_label} <span class="cz-badge cz-badge-info" style="margin-left: 0.5rem;">{identity_v01}</span></p>
<p class="cz-muted" style="margin: 0; font-size: 0.82rem;">{identity_help}</p>
</div>
</div>
</details>
</li>"#,
        slug = slug,
        name = html_escape(&name),
        role = html_escape(&role),
        badge_class = badge_class,
        badge_text = html_escape(badge_text),
        configured_label = html_escape(&configured_label),
        unavailable_block = unavailable_block,
        identity_label = html_escape(&identity_label),
        identity_help = html_escape(&identity_help),
        identity_v01 = html_escape(&identity_v01),
        port_label = html_escape(&port_label),
        data_dir_label = html_escape(&data_dir_label),
        service_name_label = html_escape(&service_name_label),
        version_label = html_escape(&version_label),
        default_port = default_port,
        default_service = html_escape(&default_service),
        disabled_attr = disabled_attr,
    )
}

/// Render the `/install` page: the unified whole-stack install form.
/// One card per component (each with inline service-config + a
/// v0.1-placeholder identity disclosure), one Install button at the
/// bottom that POSTs back to `/install` and runs every component in
/// dependency order.
///
/// `active` lists any install jobs currently in flight; one banner per
/// job renders above the form linking back to `/install/job/{id}` so
/// operators can re-attach the wizard after navigating away.
///
/// Per-component pages (`/install/postgres`, `/install/kanidm`, ...)
/// remain available for power users and CI scripts but are no longer
/// linked from the hub.
#[must_use]
pub fn render_install_hub(localizer: &Localizer, active: &[ActiveJob]) -> String {
    let title = localizer.t("ui-install-hub-title");
    let intro = localizer.t("ui-install-hub-intro");
    let install_all_button = localizer.t("ui-install-all-button");
    let install_all_helper = localizer.t("ui-install-all-helper");

    let cards: String = COMPONENTS
        .iter()
        .map(|c| render_unified_component_card(localizer, c))
        .collect();

    let platform_banner = render_platform_banner(localizer);
    let active_banner = render_active_jobs_banner(localizer, active);
    let csrf = auth::csrf_input();

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{platform_banner}
{active_banner}
<form method="post" action="/install" style="display: contents;">
{csrf}
<section class="cz-section" style="max-width: 60rem;">
<ul class="cz-install-rows" style="list-style: none; padding: 0; margin: 0;">{cards}</ul>
</section>
<section class="cz-section" style="max-width: 60rem;">
<div class="cz-card">
<p class="cz-card-body" style="margin: 0 0 1rem;">{install_all_helper}</p>
<button type="submit" class="cz-btn cz-btn-primary">{install_all_button}</button>
</div>
</section>
</form>
<script>
// Light the per-row "Reviewed" badge once every enabled input in the
// row holds a non-empty value. Watches input events so the badge
// reflects the live field state -- blank rows stay un-badged so the
// operator can scan vertically and see which components still need
// attention, but a fully-filled row tells them "this one is done,
// move on to the next".
(function () {{
  const rows = document.querySelectorAll(".cz-install-details");
  rows.forEach(row => {{
    const inputs = row.querySelectorAll("input.cz-input:not([disabled])");
    const badge = row.querySelector(".cz-install-configured-badge");
    if (!badge || inputs.length === 0) return;
    function recompute() {{
      const allFilled = Array.from(inputs).every(i => i.value.trim() !== "");
      badge.style.display = allFilled ? "inline-block" : "none";
    }}
    inputs.forEach(i => i.addEventListener("input", recompute));
    recompute();
  }});
}})();
</script>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        platform_banner = platform_banner,
        active_banner = active_banner,
        cards = cards,
        install_all_helper = html_escape(&install_all_helper),
        install_all_button = html_escape(&install_all_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render one banner per active install job. Empty string when no
/// jobs are in flight. The banner links to `/install/job/{id}` so
/// the operator can pick up wherever they left off after navigating
/// away or refreshing the browser.
fn render_active_jobs_banner(localizer: &Localizer, active: &[ActiveJob]) -> String {
    if active.is_empty() {
        return String::new();
    }
    let title = localizer.t("ui-install-active-title");
    let resume = localizer.t("ui-install-active-resume");
    let running_word = localizer.t("ui-install-active-running");

    let rows: String = active
        .iter()
        .map(|j| {
            let progress_line = if j.components_total > 0 {
                let running = j
                    .running_slug
                    .as_deref()
                    .map(|s| format!(", {s} {running_word}"))
                    .unwrap_or_default();
                format!(
                    "{}/{} components{running}",
                    j.components_done, j.components_total,
                    running = running,
                )
            } else {
                "single component".into()
            };
            format!(
                r#"<div class="cz-card-body" style="display: flex; align-items: center; justify-content: space-between; gap: 1rem; margin: 0 0 0.5rem;">
<span class="cz-muted" style="font-size: 0.85rem;">{progress_line}</span>
<a class="cz-btn" href="/install/job/{job_id}">{resume}</a>
</div>"#,
                progress_line = html_escape(&progress_line),
                job_id = html_escape(&j.id),
                resume = html_escape(&resume),
            )
        })
        .collect();

    format!(
        r#"<section class="cz-section" style="max-width: 60rem;">
<div class="cz-card" style="border-color: rgba(159, 232, 196, 0.45);">
<p class="cz-card-body" style="margin: 0 0 0.75rem;"><span class="cz-badge cz-badge-warn">{title}</span></p>
{rows}
</div>
</section>"#,
        title = html_escape(&title),
        rows = rows,
    )
}

/// Guard helper: returns `Err(Response)` with a clean unsupported-OS
/// result page when the host isn't a supported Linux distro. Install
/// POST handlers call this before doing anything else so the operator
/// never lands on a half-finished install on the wrong platform.
#[allow(clippy::result_large_err)]
fn guard_supported_os(localizer: &Localizer) -> Result<(), Response> {
    let info = computeza_driver_native::os_detect::detect();
    if info.supported {
        return Ok(());
    }
    let title = localizer.t("ui-platform-banner-unsupported");
    let supported = localizer.t("ui-platform-supported-distros");
    let detected = info.distro_name.as_deref().unwrap_or("Unknown OS");
    let arch = &info.arch;
    let reason = info.unsupported_reason.as_deref().unwrap_or("");
    let detail =
        format!("{title}\n\n{detected} ({arch})\n\n{reason}\n\nSupported platforms: {supported}");
    Err(Html(render_install_result(localizer, false, &detail)).into_response())
}

/// Render the host-OS banner above the install hub. Friendly green
/// "Detected: Ubuntu 24.04 (x86_64)" on supported Linux, prominent
/// warning card on macOS / Windows / Alpine / aarch64 hosts pointing
/// the operator at a supported distro.
fn render_platform_banner(localizer: &Localizer) -> String {
    let info = computeza_driver_native::os_detect::detect();
    let supported_distros = localizer.t("ui-platform-supported-distros");
    if info.supported {
        let label = info.distro_name.as_deref().unwrap_or("Linux").to_string();
        let detected_label = localizer.t("ui-platform-banner-supported");
        format!(
            r#"<section class="cz-section">
<div class="cz-card" style="border-color: rgba(159, 232, 196, 0.45);">
<p class="cz-card-body" style="margin: 0; display: flex; align-items: center; gap: 0.75rem;">
<span class="cz-badge cz-badge-ok">{detected}</span>
<span class="cz-strong">{label}</span>
<span class="cz-muted">({arch})</span>
</p>
</div>
</section>"#,
            detected = html_escape(&detected_label),
            label = html_escape(&label),
            arch = html_escape(&info.arch),
        )
    } else {
        let detected_label = info
            .distro_name
            .as_deref()
            .unwrap_or("Unknown OS")
            .to_string();
        let title = localizer.t("ui-platform-banner-unsupported");
        let reason = info
            .unsupported_reason
            .clone()
            .unwrap_or_else(|| "Unsupported platform".into());
        format!(
            r#"<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0 0 0.75rem; display: flex; align-items: center; gap: 0.75rem;">
<span class="cz-badge cz-badge-warn">{title}</span>
<span class="cz-strong">{detected}</span>
<span class="cz-muted">({arch})</span>
</p>
<p class="cz-muted" style="margin: 0 0 0.5rem;">{reason}</p>
<p class="cz-muted" style="margin: 0; font-size: 0.85rem;"><strong>Supported:</strong> {distros}</p>
</div>
</section>"#,
            title = html_escape(&title),
            detected = html_escape(&detected_label),
            arch = html_escape(&info.arch),
            reason = html_escape(&reason),
            distros = html_escape(&supported_distros),
        )
    }
}

/// Probe a list of [`SystemCommand`] names against `$PATH` and return
/// the rows that aren't found. Separated from the renderer so the
/// pure-string formatter is testable without touching the host
/// environment.
///
/// [`SystemCommand`]: computeza_driver_native::prerequisites::SystemCommand
fn missing_prerequisites(
    required: &[&str],
) -> Vec<computeza_driver_native::prerequisites::SystemCommand> {
    use computeza_driver_native::prerequisites::{which_on_path, SYSTEM_COMMANDS};
    required
        .iter()
        .filter(|name| which_on_path(name).is_none())
        .filter_map(|name| SYSTEM_COMMANDS.iter().find(|c| c.name == *name).copied())
        .collect()
}

/// Render the missing-prerequisite warning card for an install wizard.
///
/// `missing` is the pre-computed slice of [`SystemCommand`] rows whose
/// `name` did not resolve on `$PATH`. Returns an empty string when the
/// slice is empty so the caller can `format!("{prereq_banner}...")`
/// unconditionally.
///
/// [`SystemCommand`]: computeza_driver_native::prerequisites::SystemCommand
fn render_prerequisite_banner(
    localizer: &Localizer,
    missing: &[computeza_driver_native::prerequisites::SystemCommand],
) -> String {
    if missing.is_empty() {
        return String::new();
    }

    let title = localizer.t("ui-prerequisite-banner-title");
    let intro = localizer.t("ui-prerequisite-banner-intro");
    let needed_for = localizer.t("ui-prerequisite-banner-needed-for");
    let install_hint_label = localizer.t("ui-prerequisite-banner-install-hint");

    let rows: String = missing
        .iter()
        .map(|c| {
            format!(
                r#"<li style="margin-top: 0.75rem;">
<p style="margin: 0 0 0.25rem;"><span class="cz-badge cz-badge-warn">{name}</span></p>
<p class="cz-muted" style="margin: 0 0 0.25rem; font-size: 0.85rem;"><strong>{needed_for}:</strong> {required_for}</p>
<p class="cz-muted" style="margin: 0; font-size: 0.85rem;"><strong>{install_hint_label}:</strong> <code>{install_hint}</code></p>
</li>"#,
                name = html_escape(c.name),
                needed_for = html_escape(&needed_for),
                required_for = html_escape(c.required_for),
                install_hint_label = html_escape(&install_hint_label),
                install_hint = html_escape(c.install_hint),
            )
        })
        .collect();

    format!(
        r#"<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0 0 0.5rem;">
<span class="cz-badge cz-badge-warn">{title}</span>
</p>
<p class="cz-muted" style="margin: 0 0 0.5rem;">{intro}</p>
<ul style="list-style: none; padding-left: 0; margin: 0;">
{rows}
</ul>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        rows = rows,
    )
}

/// Render the per-component "coming soon" page. Shown for every
/// component on the hub that isn't yet wired up. Includes a short
/// localized blurb about what the component will do and a link back
/// to the install hub.
#[must_use]
pub fn render_install_coming_soon(localizer: &Localizer, slug: &str) -> String {
    let title = localizer.t("ui-install-coming-soon-title");
    let back = localizer.t("ui-install-coming-soon-back");

    let component = COMPONENTS.iter().find(|c| c.slug == slug);
    let display_name = match component {
        Some(c) => localizer.t(c.name_key),
        None => slug.to_string(),
    };
    let role = component.map(|c| localizer.t(c.role_key));

    let body_copy = localizer.t("ui-install-coming-soon-body");

    let role_block = match role {
        Some(r) => format!(
            r#"<p class="cz-muted" style="margin-top: 1rem;">{}</p>"#,
            html_escape(&r)
        ),
        None => String::new(),
    };

    let body = format!(
        r#"<section class="cz-hero">
<span class="cz-tag">{display_name}</span>
<h1>{title}</h1>
<p>{body_copy}</p>
{role_block}
</section>
<section class="cz-section">
<a class="cz-btn" href="/install">&lt;-&nbsp;{back}</a>
</section>"#,
        display_name = html_escape(&display_name),
        title = html_escape(&title),
        body_copy = html_escape(&body_copy),
        role_block = role_block,
        back = html_escape(&back),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// to the CLI's `computeza install postgres`.
///
/// `detected` is the host-state survey from
/// `computeza_driver_native::detect::postgres()`. The wizard renders
/// it as a "Detected installs" card and feeds it into
/// `detect::smart_defaults` so the form's placeholders show
/// non-colliding suggestions (port shifted past 5432, service name
/// suffixed with the major version, data-dir leaf suffixed similarly).
#[must_use]
pub fn render_install(
    localizer: &Localizer,
    detected: &[computeza_driver_native::detect::DetectedInstall],
) -> String {
    let title = localizer.t("ui-install-title");
    let intro = localizer.t("ui-install-intro");
    let target_label = localizer.t("ui-install-target-label");
    let option_postgres = localizer.t("ui-install-postgres");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    // Per-OS pinned versions. Linux + macOS still use whatever the
    // host package manager provides; only Windows has the
    // download-from-EDB path with multiple pinned versions today.
    let version_options = postgres_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    // Smart defaults: when something is already installed, the
    // wizard's placeholders shift to non-colliding values so the
    // operator can install a second instance without typing.
    let default_version_major = version_options
        .first()
        .map(|v| v.value.split('.').next().unwrap_or("").to_string())
        .unwrap_or_default();
    let defaults = computeza_driver_native::detect::smart_defaults(
        detected,
        if default_version_major.is_empty() {
            None
        } else {
            Some(&default_version_major)
        },
    );
    let port_placeholder = format!("{}", defaults.port);
    let service_name_placeholder = defaults.service_name();
    let root_dir_placeholder = root_dir_placeholder_for_leaf(&defaults.data_dir_leaf());

    // "Detected installs" card. Empty list collapses to a single-line
    // hint so the section doesn't dominate the page for first-time
    // operators.
    let detected_title = localizer.t("ui-install-detected-title");
    let detected_empty = localizer.t("ui-install-detected-empty");
    let detected_hint = localizer.t("ui-install-detected-hint");
    let detected_html = if detected.is_empty() {
        format!(
            r#"<p class="cz-card-body" style="margin: 0;">{}</p>"#,
            html_escape(&detected_empty)
        )
    } else {
        let rows: String = detected
            .iter()
            .map(|d| {
                let badge_class = if d.owner.eq_ignore_ascii_case("computeza") {
                    "cz-badge cz-badge-info"
                } else {
                    "cz-badge cz-badge-warn"
                };
                format!(
                    r#"<li style="margin-bottom: 0.4rem;"><span class="{badge_class}">{owner}</span> <span class="cz-cell-mono">{summary}</span></li>"#,
                    badge_class = badge_class,
                    owner = html_escape(&d.owner),
                    summary = html_escape(&d.summary()),
                )
            })
            .collect();
        format!(
            r#"<ul style="list-style: none; padding: 0; margin: 0;">{rows}</ul>
<p class="cz-muted" style="margin: 0.75rem 0 0; font-size: 0.85rem;">{hint}</p>"#,
            hint = html_escape(&detected_hint),
        )
    };
    let detected_section = format!(
        r#"<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<h3 style="margin: 0 0 0.75rem;">{title}</h3>
{body}
</div>
</section>"#,
        title = html_escape(&detected_title),
        body = detected_html,
    );

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{detected_section}
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/postgres" class="cz-form" style="max-width: none;">
<label for="component">{target_label}</label>
<select id="component" name="component" class="cz-select">
<option value="postgres">{option_postgres}</option>
</select>

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help}</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/postgres/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        target_label = html_escape(&target_label),
        option_postgres = html_escape(&option_postgres),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(&port_placeholder),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(&service_name_placeholder),
        advanced_label = html_escape(&advanced_label),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
        detected_section = detected_section,
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the `/install/postgres/uninstall` confirmation page. The
/// operator clicks the destructive button here to roll back the
/// install. POSTing back to the same URL runs the teardown.
#[must_use]
pub fn render_uninstall_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-title");
    let intro = localizer.t("ui-uninstall-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/postgres/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the `/install/kanidm` wizard form. Mirrors the postgres
/// wizard shape (port + advanced disclosure with data-dir + service
/// name + version dropdown) but posts to `/install/kanidm`.
#[must_use]
pub fn render_install_kanidm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-kanidm-title");
    let intro = localizer.t("ui-install-kanidm-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = kanidm_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "8443";
    let service_name_placeholder = "computeza-kanidm";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("kanidm");

    // No host prereqs are surfaced for kanidm anymore: cargo is
    // installed by prerequisites::ensure_rust_toolchain when missing,
    // and the TLS cert step uses rcgen (pure Rust) instead of
    // shelling out to openssl. The banner-rendering call stays so
    // future per-component host deps can hook in here cleanly.
    let prereq_banner = render_prerequisite_banner(localizer, &missing_prerequisites(&[]));

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{prereq_banner}
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/kanidm" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="kanidm" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help}</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/kanidm/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the kanidm uninstall confirmation page.
#[must_use]
pub fn render_uninstall_kanidm_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-kanidm-title");
    let intro = localizer.t("ui-uninstall-kanidm-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/kanidm/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/kanidm">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the `/install/garage` wizard form. Mirrors the kanidm
/// wizard shape (port + advanced disclosure).
#[must_use]
pub fn render_install_garage(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-garage-title");
    let intro = localizer.t("ui-install-garage-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = garage_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "3900";
    let service_name_placeholder = "computeza-garage";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("garage");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/garage" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="garage" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Garage binds four ports starting at this value: S3 API (this port), RPC (+1), web (+2), admin (+3).</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/garage/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the garage uninstall confirmation page.
#[must_use]
pub fn render_uninstall_garage_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-garage-title");
    let intro = localizer.t("ui-uninstall-garage-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/garage/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/garage">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

fn garage_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::garage;
        garage::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Garage {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Garage (Linux only for v0.0.x)".into(),
        }]
    }
}

/// Render the `/install/openfga` wizard form. Mirrors the garage
/// wizard shape (single port input + advanced disclosure).
#[must_use]
pub fn render_install_openfga(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-openfga-title");
    let intro = localizer.t("ui-install-openfga-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = openfga_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "8080";
    let service_name_placeholder = "computeza-openfga";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("openfga");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/openfga" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="openfga" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} OpenFGA binds HTTP on this port and gRPC on port+1.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/openfga/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the openfga uninstall confirmation page.
#[must_use]
pub fn render_uninstall_openfga_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-openfga-title");
    let intro = localizer.t("ui-uninstall-openfga-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/openfga/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/openfga">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

fn openfga_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::openfga;
        openfga::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "OpenFGA {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "OpenFGA (Linux only for v0.0.x)".into(),
        }]
    }
}

/// Render the `/install/qdrant` wizard form.
#[must_use]
pub fn render_install_qdrant(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-qdrant-title");
    let intro = localizer.t("ui-install-qdrant-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = qdrant_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "6333";
    let service_name_placeholder = "computeza-qdrant";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("qdrant");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/qdrant" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="qdrant" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Qdrant binds REST on this port and gRPC on port+1.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/qdrant/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the qdrant uninstall confirmation page.
#[must_use]
pub fn render_uninstall_qdrant_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-qdrant-title");
    let intro = localizer.t("ui-uninstall-qdrant-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/qdrant/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/qdrant">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the GreptimeDB install wizard.
#[must_use]
pub fn render_install_greptime(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-greptime-title");
    let intro = localizer.t("ui-install-greptime-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = greptime_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "4000";
    let service_name_placeholder = "computeza-greptime";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("greptime");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/greptime" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="greptime" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Greptime binds HTTP on this port; the gRPC and MySQL/PostgreSQL listeners bind on adjacent ports.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/greptime/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the greptime uninstall confirmation page.
#[must_use]
pub fn render_uninstall_greptime_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-greptime-title");
    let intro = localizer.t("ui-uninstall-greptime-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/greptime/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/greptime">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the Lakekeeper install wizard.
#[must_use]
pub fn render_install_lakekeeper(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-lakekeeper-title");
    let intro = localizer.t("ui-install-lakekeeper-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = lakekeeper_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "8181";
    let service_name_placeholder = "computeza-lakekeeper";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("lakekeeper");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/lakekeeper" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="lakekeeper" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Lakekeeper binds the Iceberg REST API on this port.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/lakekeeper/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the lakekeeper uninstall confirmation page.
#[must_use]
pub fn render_uninstall_lakekeeper_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-lakekeeper-title");
    let intro = localizer.t("ui-uninstall-lakekeeper-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/lakekeeper/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/lakekeeper">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the Databend install wizard.
#[must_use]
pub fn render_install_databend(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-databend-title");
    let intro = localizer.t("ui-install-databend-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = databend_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "8000";
    let service_name_placeholder = "computeza-databend";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("databend");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/databend" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="databend" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Databend binds the SQL HTTP handler on this port.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/databend/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the databend uninstall confirmation page.
#[must_use]
pub fn render_uninstall_databend_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-databend-title");
    let intro = localizer.t("ui-uninstall-databend-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/databend/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/databend">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the Grafana install wizard.
#[must_use]
pub fn render_install_grafana(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-grafana-title");
    let intro = localizer.t("ui-install-grafana-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = grafana_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "3000";
    let service_name_placeholder = "computeza-grafana";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("grafana");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/grafana" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="grafana" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Grafana binds the web UI on this port.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/grafana/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the grafana uninstall confirmation page.
#[must_use]
pub fn render_uninstall_grafana_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-grafana-title");
    let intro = localizer.t("ui-uninstall-grafana-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/grafana/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/grafana">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the Restate install wizard.
#[must_use]
pub fn render_install_restate(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-restate-title");
    let intro = localizer.t("ui-install-restate-intro");
    let port_label = localizer.t("ui-install-port-label");
    let port_help = localizer.t("ui-install-port-help");
    let version_label = localizer.t("ui-install-version-label");
    let version_help = localizer.t("ui-install-version-help");
    let data_dir_label = localizer.t("ui-install-data-dir-label");
    let data_dir_help = localizer.t("ui-install-data-dir-help");
    let service_name_label = localizer.t("ui-install-service-name-label");
    let service_name_help = localizer.t("ui-install-service-name-help");
    let advanced_label = localizer.t("ui-install-advanced");
    let button = localizer.t("ui-install-button");
    let requires_root = localizer.t("ui-install-requires-root");
    let already_installed = localizer.t("ui-install-already-installed");
    let uninstall_button = localizer.t("ui-uninstall-button");

    let version_options = restate_version_options();
    let version_options_html: String = version_options
        .iter()
        .map(|v| {
            format!(
                r#"<option value="{value}">{label}</option>"#,
                value = html_escape(&v.value),
                label = html_escape(&v.label),
            )
        })
        .collect();

    let port_placeholder = "8080";
    let service_name_placeholder = "computeza-restate";
    let root_dir_placeholder = root_dir_placeholder_for_leaf("restate");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card">
<form method="post" action="/install/restate" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="restate" />

<label for="version">{version_label}</label>
<select id="version" name="version" class="cz-select">
{version_options_html}
</select>
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{version_help}</p>

<label for="port">{port_label}</label>
<input id="port" name="port" class="cz-input" type="number" min="1" max="65535" placeholder="{port_placeholder}" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{port_help} Restate binds the ingress on this port; admin and node listeners bind on 9070 + 5122 by default.</p>

<details style="margin-top: 0.5rem;">
<summary class="cz-tag" style="cursor: pointer;">{advanced_label}</summary>
<div style="display: flex; flex-direction: column; gap: 0.9rem; margin-top: 1rem;">
<div>
<label for="root_dir" style="display: block; margin-bottom: 0.4rem;">{data_dir_label}</label>
<input id="root_dir" name="root_dir" class="cz-input" type="text" placeholder="{root_dir_placeholder}" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{data_dir_help}</p>
</div>
<div>
<label for="service_name" style="display: block; margin-bottom: 0.4rem;">{service_name_label}</label>
<input id="service_name" name="service_name" class="cz-input" type="text" placeholder="{service_name_placeholder}" pattern="[A-Za-z0-9_-]+" />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.8rem;">{service_name_help}</p>
</div>
</div>
</details>

<button type="submit" class="cz-btn cz-btn-primary" style="align-self: flex-start; margin-top: 0.5rem;">{button}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem;">{requires_root}</p>

<div class="cz-card" style="margin-top: 1.5rem;">
<p class="cz-card-body" style="margin: 0 0 1rem;">{already_installed}</p>
<form method="get" action="/install/restate/uninstall">
<button type="submit" class="cz-btn cz-btn-danger">{uninstall_button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        version_label = html_escape(&version_label),
        version_help = html_escape(&version_help),
        version_options_html = version_options_html,
        port_label = html_escape(&port_label),
        port_help = html_escape(&port_help),
        port_placeholder = html_escape(port_placeholder),
        advanced_label = html_escape(&advanced_label),
        data_dir_label = html_escape(&data_dir_label),
        data_dir_help = html_escape(&data_dir_help),
        root_dir_placeholder = html_escape(&root_dir_placeholder),
        service_name_label = html_escape(&service_name_label),
        service_name_help = html_escape(&service_name_help),
        service_name_placeholder = html_escape(service_name_placeholder),
        button = html_escape(&button),
        requires_root = html_escape(&requires_root),
        already_installed = html_escape(&already_installed),
        uninstall_button = html_escape(&uninstall_button),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the restate uninstall confirmation page.
#[must_use]
pub fn render_uninstall_restate_confirm(localizer: &Localizer) -> String {
    let title = localizer.t("ui-uninstall-restate-title");
    let intro = localizer.t("ui-uninstall-restate-intro");
    let confirm = localizer.t("ui-uninstall-confirm");
    let button = localizer.t("ui-uninstall-button");
    let cancel = localizer.t("ui-uninstall-cancel");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 1.25rem; color: var(--fail);">{confirm}</p>
<form method="post" action="/install/restate/uninstall" style="display: flex; gap: 0.75rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
<a class="cz-btn" href="/install/restate">{cancel}</a>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        confirm = html_escape(&confirm),
        button = html_escape(&button),
        cancel = html_escape(&cancel),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

fn qdrant_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::qdrant;
        qdrant::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Qdrant {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Qdrant (Linux only for v0.0.x)".into(),
        }]
    }
}

fn greptime_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::greptime;
        greptime::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "GreptimeDB {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "GreptimeDB (Linux only for v0.0.x)".into(),
        }]
    }
}

fn lakekeeper_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::lakekeeper;
        lakekeeper::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Lakekeeper {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Lakekeeper (Linux only for v0.0.x)".into(),
        }]
    }
}

fn databend_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::databend;
        databend::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Databend {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Databend (Linux only for v0.0.x)".into(),
        }]
    }
}

fn grafana_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::grafana;
        grafana::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Grafana {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Grafana (Linux only for v0.0.x)".into(),
        }]
    }
}

fn restate_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::restate;
        restate::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Restate {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec![VersionOption {
            value: String::new(),
            label: "Restate (Linux only for v0.0.x)".into(),
        }]
    }
}

fn kanidm_version_options() -> Vec<VersionOption> {
    #[cfg(target_os = "windows")]
    {
        // Windows isn't supported upstream; offer a single "(unavailable)"
        // entry that maps to version=None so install fails cleanly with
        // the upstream-not-supported error.
        vec![VersionOption {
            value: String::new(),
            label: "Kanidm (Windows not supported by upstream)".into(),
        }]
    }
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::kanidm;
        kanidm::available_versions()
            .iter()
            .enumerate()
            .map(|(i, v)| VersionOption {
                value: (*v).to_string(),
                label: format!("Kanidm {v}{}", if i == 0 { " (latest)" } else { "" }),
            })
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        use computeza_driver_native::macos::kanidm;
        kanidm::available_versions()
            .iter()
            .enumerate()
            .map(|(i, b)| VersionOption {
                value: b.version.into(),
                label: format!(
                    "Kanidm {}{}",
                    b.version,
                    if i == 0 { " (latest)" } else { "" }
                ),
            })
            .collect()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Render the in-flight wizard page. The page polls
/// `/api/install/job/{id}` every 500ms via inline JS and redirects to
/// the result page once `completed: true` lands in the snapshot.
///
/// When `p.components` is non-empty the page renders a per-component
/// checklist above the running-component progress bar; the JS poller
/// keeps each row's state badge in sync.
#[must_use]
pub fn render_install_progress(localizer: &Localizer, job_id: &str, p: &InstallProgress) -> String {
    let title = localizer.t("ui-install-title");
    let phase_label = p.phase.label();
    let ratio_pct = format!("{:.1}", p.phase_ratio() * 100.0);
    let message = html_escape(&p.message);
    let bytes_line = match (p.total_bytes, p.bytes_downloaded) {
        (Some(t), d) => format!("{} / {}", human_bytes(d), human_bytes(t)),
        (None, 0) => String::new(),
        (None, d) => human_bytes(d),
    };
    let job_id_js = html_escape(job_id);

    let initial_log_html: String = p
        .log
        .iter()
        .map(|l| format!("<div>{}</div>\n", html_escape(l)))
        .collect();

    let multi = !p.components.is_empty();
    let (hero_title, hero_intro) = if multi {
        (
            localizer.t("ui-install-progress-title-multi"),
            localizer.t("ui-install-progress-intro-multi"),
        )
    } else {
        (
            localizer.t("ui-install-progress-title-single"),
            localizer.t("ui-install-progress-intro-single"),
        )
    };

    let components_label = localizer.t("ui-install-progress-components");
    let state_pending = localizer.t("ui-install-progress-state-pending");
    let state_running = localizer.t("ui-install-progress-state-running");
    let state_done = localizer.t("ui-install-progress-state-done");
    let state_failed = localizer.t("ui-install-progress-state-failed");
    let state_pending_e = html_escape(&state_pending);
    let state_running_e = html_escape(&state_running);
    let state_done_e = html_escape(&state_done);
    let state_failed_e = html_escape(&state_failed);

    let components_block = if multi {
        let rows: String = p
            .components
            .iter()
            .map(|c| {
                let (badge_class, badge_text) = match c.state {
                    computeza_driver_native::progress::ComponentState::Pending => {
                        ("cz-badge cz-badge-info", &state_pending_e)
                    }
                    computeza_driver_native::progress::ComponentState::Running => {
                        ("cz-badge cz-badge-warn", &state_running_e)
                    }
                    computeza_driver_native::progress::ComponentState::Done => {
                        ("cz-badge cz-badge-ok", &state_done_e)
                    }
                    computeza_driver_native::progress::ComponentState::Failed => {
                        ("cz-badge cz-badge-fail", &state_failed_e)
                    }
                };
                let row_weight = match c.state {
                    computeza_driver_native::progress::ComponentState::Running => "600",
                    _ => "400",
                };
                format!(
                    r#"<li id="component-{slug}" data-state="{state_attr}" style="display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; padding: 0.45rem 0.75rem; border-radius: 0.5rem; background: rgba(255,255,255,0.04); margin-bottom: 0.35rem; font-weight: {row_weight};">
<span class="cz-component-slug">{slug_html}</span>
<span class="cz-component-state {badge_class}">{badge_text}</span>
</li>"#,
                    slug = html_escape(&c.slug),
                    slug_html = html_escape(&c.slug),
                    state_attr = format!("{:?}", c.state).to_lowercase(),
                    badge_class = badge_class,
                    badge_text = badge_text,
                    row_weight = row_weight,
                )
            })
            .collect();
        format!(
            r#"<section class="cz-section" style="max-width: 42rem;">
<div class="cz-card">
<p class="cz-card-body" style="margin: 0 0 0.6rem; font-weight: 600;">{components_label}</p>
<ul id="components" style="list-style: none; padding: 0; margin: 0;">
{rows}
</ul>
</div>
</section>"#,
            components_label = html_escape(&components_label),
            rows = rows,
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"<section class="cz-hero">
<h1>{hero_title}</h1>
<p>{hero_intro}</p>
</section>
{components_block}
<section class="cz-section" style="max-width: 42rem;">
<div class="cz-progress">
  <div class="cz-progress-phase">
    <span id="phase">{phase_label}</span>
    <span class="cz-muted" id="phase-pct">{ratio_pct}%</span>
  </div>
  <div class="cz-progress-bar"><div class="cz-progress-fill" id="bar" style="width: {ratio_pct}%;"></div></div>
  <div class="cz-progress-msg" id="message">{message}</div>
  <div class="cz-progress-bytes" id="bytes">{bytes_line}</div>
  <details style="margin-top: 1rem;" id="log-details">
    <summary class="cz-tag" style="cursor: pointer;">Show install log</summary>
    <div id="log" class="cz-pre" style="margin-top: 0.6rem; max-height: 16rem; overflow-y: auto;">{initial_log_html}</div>
  </details>
</div>
</section>
<script>
const jobId = "{job_id_js}";
let lastLogLen = {initial_log_len};
const stateLabels = {{
  "pending": "{state_pending_e}",
  "running": "{state_running_e}",
  "done": "{state_done_e}",
  "failed": "{state_failed_e}",
}};
const stateBadgeClass = {{
  "pending": "cz-badge cz-badge-info",
  "running": "cz-badge cz-badge-warn",
  "done": "cz-badge cz-badge-ok",
  "failed": "cz-badge cz-badge-fail",
}};
function fmt(n) {{
  if (n === 0) return "0 B";
  const u = ["B","KB","MB","GB","TB"];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) {{ v /= 1024; i++; }}
  return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
}}
function appendLog(lines) {{
  if (!lines || lines.length === 0) return;
  const el = document.getElementById("log");
  const wasAtBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 4;
  for (const line of lines) {{
    const div = document.createElement("div");
    div.textContent = line;
    el.appendChild(div);
  }}
  if (wasAtBottom) {{ el.scrollTop = el.scrollHeight; }}
}}
function updateComponents(components) {{
  if (!Array.isArray(components)) return;
  for (const c of components) {{
    const row = document.getElementById("component-" + c.slug);
    if (!row) continue;
    if (row.dataset.state === c.state) continue;
    row.dataset.state = c.state;
    row.style.fontWeight = (c.state === "running") ? "600" : "400";
    const badge = row.querySelector(".cz-component-state");
    if (badge) {{
      badge.className = "cz-component-state " + (stateBadgeClass[c.state] || "cz-badge cz-badge-info");
      badge.textContent = stateLabels[c.state] || c.state;
    }}
  }}
}}
async function poll() {{
  try {{
    const r = await fetch(`/api/install/job/${{jobId}}`, {{cache: "no-store"}});
    if (!r.ok) {{ setTimeout(poll, 1500); return; }}
    const p = await r.json();
    document.getElementById("phase").textContent = p.phase_label || p.phase;
    document.getElementById("message").textContent = p.message || "";
    const pct = (p.phase_ratio * 100).toFixed(1);
    document.getElementById("bar").style.width = pct + "%";
    document.getElementById("phase-pct").textContent = pct + "%";
    let bytes = "";
    if (p.total_bytes) {{
      bytes = fmt(p.bytes_downloaded) + " / " + fmt(p.total_bytes);
    }} else if (p.bytes_downloaded > 0) {{
      bytes = fmt(p.bytes_downloaded);
    }}
    document.getElementById("bytes").textContent = bytes;
    if (Array.isArray(p.log) && p.log.length > lastLogLen) {{
      appendLog(p.log.slice(lastLogLen));
      lastLogLen = p.log.length;
    }}
    updateComponents(p.components);
    if (p.completed) {{
      window.location.href = `/install/job/${{jobId}}`;
    }} else {{
      setTimeout(poll, 500);
    }}
  }} catch (e) {{
    setTimeout(poll, 1500);
  }}
}}
poll();
</script>"#,
        hero_title = html_escape(&hero_title),
        hero_intro = html_escape(&hero_intro),
        components_block = components_block,
        initial_log_len = p.log.len(),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Format a byte count with a binary-prefix unit. Mirrors the JS
/// `fmt` function used by the wizard so the first render (server-side)
/// matches the live updates.
fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if n == 0 {
        return "0 B".into();
    }
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Render `GET /login`. `failure_message` populates a warn-styled
/// card above the form when a previous POST attempt failed (wrong
/// credentials, validation error, etc).
#[must_use]
pub fn render_login(localizer: &Localizer, next: &str, failure_message: Option<&str>) -> String {
    let title = localizer.t("ui-login-title");
    let intro = localizer.t("ui-login-intro");
    let username_label = localizer.t("ui-login-username");
    let password_label = localizer.t("ui-login-password");
    let submit = localizer.t("ui-login-submit");
    let no_account = localizer.t("ui-login-no-account");
    let go_to_setup = localizer.t("ui-login-go-to-setup");
    let back_to_landing = localizer.t("ui-login-back-to-landing");

    let failure_block = match failure_message {
        Some(msg) => format!(
            r#"<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55); margin-bottom: 1rem;">
<p class="cz-card-body" style="margin: 0; color: var(--fail);">{}</p>
</div>"#,
            html_escape(msg)
        ),
        None => String::new(),
    };

    let body = format!(
        r#"<section class="cz-hero" style="text-align: center;">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 26rem; margin: 0 auto;">
{failure_block}
<div class="cz-card">
<form method="post" action="/login" class="cz-form" style="max-width: none;">
<input type="hidden" name="next" value="{next}" />
<label for="login-username">{username_label}</label>
<input id="login-username" name="username" class="cz-input" type="text" autocomplete="username" required />
<label for="login-password">{password_label}</label>
<input id="login-password" name="password" class="cz-input" type="password" autocomplete="current-password" required />
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">{submit}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 1rem; font-size: 0.85rem; text-align: center;">
{no_account} <a href="/setup">{go_to_setup}</a>
</p>
<p class="cz-muted" style="margin-top: 0.5rem; font-size: 0.85rem; text-align: center;">
<a href="/">{back_to_landing}</a>
</p>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        next = html_escape(next),
        failure_block = failure_block,
        username_label = html_escape(&username_label),
        password_label = html_escape(&password_label),
        submit = html_escape(&submit),
        no_account = html_escape(&no_account),
        go_to_setup = html_escape(&go_to_setup),
        back_to_landing = html_escape(&back_to_landing),
    );
    render_shell(localizer, &title, NavLink::None, &body)
}

/// Render `GET /setup` -- the first-boot operator-creation form.
/// `failure_message` populates a warn-styled card above the form when
/// the previous POST attempt failed (mismatched passwords, validation
/// error, etc).
#[must_use]
pub fn render_setup(localizer: &Localizer, failure_message: Option<&str>) -> String {
    let title = localizer.t("ui-setup-title");
    let intro = localizer.t("ui-setup-intro");
    let username = localizer.t("ui-setup-username");
    let username_help = localizer.t("ui-setup-username-help");
    let password = localizer.t("ui-setup-password");
    let password_help = localizer.t("ui-setup-password-help");
    let password_confirm = localizer.t("ui-setup-password-confirm");
    let submit = localizer.t("ui-setup-submit");

    let failure_block = match failure_message {
        Some(msg) => format!(
            r#"<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55); margin-bottom: 1rem;">
<p class="cz-card-body" style="margin: 0; color: var(--fail);">{}</p>
</div>"#,
            html_escape(msg)
        ),
        None => String::new(),
    };

    let body = format!(
        r#"<section class="cz-hero" style="text-align: center;">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 32rem; margin: 0 auto;">
{failure_block}
<div class="cz-card">
<form method="post" action="/setup" class="cz-form" style="max-width: none;">
<label for="setup-username">{username}</label>
<input id="setup-username" name="username" class="cz-input" type="text" autocomplete="username" required pattern="[A-Za-z0-9_.\-]+" maxlength="64" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{username_help}</p>
<label for="setup-password">{password}</label>
<input id="setup-password" name="password" class="cz-input" type="password" autocomplete="new-password" minlength="12" required />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{password_help}</p>
<label for="setup-password-confirm">{password_confirm}</label>
<input id="setup-password-confirm" name="password_confirm" class="cz-input" type="password" autocomplete="new-password" minlength="12" required />
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">{submit}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        failure_block = failure_block,
        username = html_escape(&username),
        username_help = html_escape(&username_help),
        password = html_escape(&password),
        password_help = html_escape(&password_help),
        password_confirm = html_escape(&password_confirm),
        submit = html_escape(&submit),
    );
    render_shell(localizer, &title, NavLink::None, &body)
}

/// Render `GET /account` -- operator account details with a Sign out
/// form. Auth-required surface.
#[must_use]
pub fn render_account(localizer: &Localizer, session: &auth::Session) -> String {
    let title = localizer.t("ui-account-title");
    let intro = localizer.t("ui-account-intro");
    let username_label = localizer.t("ui-account-username");
    let session_since = localizer.t("ui-account-session-since");
    let logout = localizer.t("ui-nav-logout");

    let body = format!(
        r#"<section class="cz-hero" style="text-align: center;">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 32rem; margin: 0 auto;">
<div class="cz-card">
<dl class="cz-dl">
<dt>{username_label}</dt><dd><code>{username}</code></dd>
<dt>{session_since}</dt><dd><code>{created_at}</code></dd>
</dl>
<form method="post" action="/logout" style="margin-top: 1.25rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{logout}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        username_label = html_escape(&username_label),
        session_since = html_escape(&session_since),
        logout = html_escape(&logout),
        username = html_escape(&session.username),
        created_at = html_escape(&session.created_at.to_rfc3339()),
    );
    render_shell(localizer, &title, NavLink::Account, &body)
}

/// Render `GET /admin/operators` -- the operator-management page.
/// Lists every operator with edit + delete forms per row, plus a
/// "create new operator" form at the bottom. `current_username`
/// populates the disabled-state on the row for the operator who is
/// currently signed in (prevents self-delete from the UI; the
/// handler enforces this too).
#[must_use]
pub fn render_admin_operators(
    localizer: &Localizer,
    operators: &[auth::OperatorRecord],
    current_username: &str,
    error_message: Option<&str>,
) -> String {
    let title = localizer.t("ui-admin-operators-title");
    let intro = localizer.t("ui-admin-operators-intro");
    let col_user = localizer.t("ui-admin-operators-col-username");
    let col_groups = localizer.t("ui-admin-operators-col-groups");
    let col_created = localizer.t("ui-admin-operators-col-created");
    let col_actions = localizer.t("ui-admin-operators-col-actions");
    let delete = localizer.t("ui-admin-operators-delete");
    let confirm = localizer.t("ui-admin-operators-delete-confirm");
    let edit_groups = localizer.t("ui-admin-operators-edit-groups");
    let new_heading = localizer.t("ui-admin-operators-new-heading");
    let new_username = localizer.t("ui-admin-operators-new-username");
    let new_password = localizer.t("ui-admin-operators-new-password");
    let new_password_help = localizer.t("ui-admin-operators-new-password-help");
    let new_groups = localizer.t("ui-admin-operators-new-groups");
    let new_submit = localizer.t("ui-admin-operators-new-submit");

    let rows: String = operators
        .iter()
        .map(|op| {
            let groups_str = op.groups.join(", ");
            let is_self = op.username == current_username;
            let delete_button = if is_self {
                format!(
                    r#"<button type="submit" class="cz-btn cz-btn-danger" disabled title="self">{delete}</button>"#,
                    delete = html_escape(&delete),
                )
            } else {
                format!(
                    r#"<button type="submit" class="cz-btn cz-btn-danger">{delete}</button>"#,
                    delete = html_escape(&delete),
                )
            };
            format!(
                r#"<tr>
<td><code>{username}</code></td>
<td>
<form method="post" action="/admin/operators/{username_enc}/groups" style="display: flex; gap: 0.4rem; align-items: center;">
<input type="hidden" name="csrf_token" value="" />
<input type="text" name="groups" value="{groups}" class="cz-input" style="margin: 0; padding: 0.3rem 0.5rem; font-size: 0.82rem; min-width: 12rem;" pattern="[A-Za-z0-9_,\- ]+" />
<button type="submit" class="cz-btn" style="padding: 0.3rem 0.6rem; font-size: 0.78rem;">{edit_groups}</button>
</form>
</td>
<td class="cz-cell-mono cz-cell-dim">{created}</td>
<td>
<form method="post" action="/admin/operators/{username_enc}/delete" onsubmit="return confirm('{confirm}');" style="margin: 0;">
<input type="hidden" name="csrf_token" value="" />
{delete_button}
</form>
</td>
</tr>"#,
                username = html_escape(&op.username),
                username_enc = urlencoding_min(&op.username),
                groups = html_escape(&groups_str),
                edit_groups = html_escape(&edit_groups),
                created = html_escape(&op.created_at.to_rfc3339()),
                confirm = html_escape(&confirm),
                delete_button = delete_button,
            )
        })
        .collect();

    let error_block = match error_message {
        Some(msg) => format!(
            r#"<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55); margin-bottom: 1rem;">
<p class="cz-card-body" style="margin: 0; color: var(--fail);">{}</p>
</div>"#,
            html_escape(msg)
        ),
        None => String::new(),
    };

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
{error_block}
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{col_user}</th><th>{col_groups}</th><th>{col_created}</th><th>{col_actions}</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</div>
</section>
<section class="cz-section" style="max-width: 32rem;">
<div class="cz-card">
<h3 style="margin: 0 0 0.85rem;">{new_heading}</h3>
<form method="post" action="/admin/operators" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<label for="new-username">{new_username}</label>
<input id="new-username" name="username" class="cz-input" type="text" required pattern="[A-Za-z0-9_.\-]+" maxlength="64" />
<label for="new-password">{new_password}</label>
<input id="new-password" name="password" class="cz-input" type="password" minlength="12" required autocomplete="new-password" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{new_password_help}</p>
<label for="new-groups">{new_groups}</label>
<input id="new-groups" name="groups" class="cz-input" type="text" value="operators" pattern="[A-Za-z0-9_,\- ]+" />
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">{new_submit}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        error_block = error_block,
        col_user = html_escape(&col_user),
        col_groups = html_escape(&col_groups),
        col_created = html_escape(&col_created),
        col_actions = html_escape(&col_actions),
        rows = rows,
        new_heading = html_escape(&new_heading),
        new_username = html_escape(&new_username),
        new_password = html_escape(&new_password),
        new_password_help = html_escape(&new_password_help),
        new_groups = html_escape(&new_groups),
        new_submit = html_escape(&new_submit),
    );
    render_shell(localizer, &title, NavLink::Operators, &body)
}

/// Render `GET /admin/groups` -- read-only listing of built-in groups
/// and the permissions each carries. v0.0.x ships admins / operators
/// / viewers; custom groups are v0.1+.
#[must_use]
pub fn render_admin_groups(localizer: &Localizer) -> String {
    let title = localizer.t("ui-admin-groups-title");
    let intro = localizer.t("ui-admin-groups-intro");
    let col_name = localizer.t("ui-admin-groups-col-name");
    let col_perms = localizer.t("ui-admin-groups-col-perms");

    let rows: String = auth::BUILTIN_GROUPS
        .iter()
        .map(|(name, perms)| {
            let perm_str = perms
                .iter()
                .map(|p| p.label())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                r#"<tr><td><code>{name}</code></td><td>{perms}</td></tr>"#,
                name = html_escape(name),
                perms = html_escape(&perm_str),
            )
        })
        .collect();

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{col_name}</th><th>{col_perms}</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        col_name = html_escape(&col_name),
        col_perms = html_escape(&col_perms),
        rows = rows,
    );
    render_shell(localizer, &title, NavLink::Groups, &body)
}

/// Render `GET /audit` -- the audit-log viewer. Passing `None` for
/// `events` indicates the audit log is not attached on this server
/// (e.g. the smoke-test surface). Passing `Some(&[])` renders an
/// empty-state card so the operator knows the log is wired but no
/// events have been written yet.
#[must_use]
pub fn render_audit(
    localizer: &Localizer,
    events: Option<&[computeza_audit::AuditEvent]>,
    verifying_key_b64: Option<&str>,
) -> String {
    let title = localizer.t("ui-audit-title");
    let intro = localizer.t("ui-audit-intro");
    let empty = localizer.t("ui-audit-empty");
    let missing = localizer.t("ui-audit-missing");
    let col_seq = localizer.t("ui-audit-col-seq");
    let col_ts = localizer.t("ui-audit-col-timestamp");
    let col_actor = localizer.t("ui-audit-col-actor");
    let col_action = localizer.t("ui-audit-col-action");
    let col_resource = localizer.t("ui-audit-col-resource");
    let verifying_label = localizer.t("ui-audit-verifying-key");

    let body = match events {
        None => format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0; color: var(--warn);">{missing}</p>
</div>
</section>"#,
            title = html_escape(&title),
            intro = html_escape(&intro),
            missing = html_escape(&missing),
        ),
        Some([]) => format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card"><p class="cz-card-body" style="margin: 0;">{empty}</p></div>
</section>"#,
            title = html_escape(&title),
            intro = html_escape(&intro),
            empty = html_escape(&empty),
        ),
        Some(events) => {
            let rows: String = events
                .iter()
                .map(|e| {
                    let resource = e.body.resource.as_deref().unwrap_or("-");
                    format!(
                        r#"<tr>
<td class="cz-cell-mono">{seq}</td>
<td class="cz-cell-mono cz-cell-dim">{ts}</td>
<td>{actor}</td>
<td><span class="cz-badge cz-badge-info">{action}</span></td>
<td class="cz-cell-mono">{resource}</td>
</tr>"#,
                        seq = e.body.seq,
                        ts = html_escape(&e.body.timestamp.to_rfc3339()),
                        actor = html_escape(&e.body.actor),
                        action = html_escape(&format!("{:?}", e.body.action)),
                        resource = html_escape(resource),
                    )
                })
                .collect();
            let key_block = match verifying_key_b64 {
                Some(k) => format!(
                    r#"<p class="cz-muted" style="margin-top: 1rem; font-size: 0.82rem;"><strong>{verifying_label}:</strong> <code>{k}</code></p>"#,
                    verifying_label = html_escape(&verifying_label),
                    k = html_escape(k),
                ),
                None => String::new(),
            };
            format!(
                r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{col_seq}</th><th>{col_ts}</th><th>{col_actor}</th><th>{col_action}</th><th>{col_resource}</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
{key_block}
</div>
</section>"#,
                title = html_escape(&title),
                intro = html_escape(&intro),
                col_seq = html_escape(&col_seq),
                col_ts = html_escape(&col_ts),
                col_actor = html_escape(&col_actor),
                col_action = html_escape(&col_action),
                col_resource = html_escape(&col_resource),
                rows = rows,
                key_block = key_block,
            )
        }
    };

    render_shell(localizer, &title, NavLink::Audit, &body)
}

/// Render `GET /setup` when an operator account already exists.
/// Carries a link onward to `/login` so the operator can sign in.
#[must_use]
pub fn render_setup_already_done(localizer: &Localizer) -> String {
    let title = localizer.t("ui-setup-title");
    let msg = localizer.t("ui-setup-already-done");
    let sign_in = localizer.t("ui-login-submit");

    let body = format!(
        r#"<section class="cz-hero" style="text-align: center;">
<h1>{title}</h1>
<p>{msg}</p>
<div class="cz-cta-row">
<a class="cz-btn cz-btn-primary cz-btn-lg" href="/login">{sign_in}</a>
</div>
</section>"#,
        title = html_escape(&title),
        msg = html_escape(&msg),
        sign_in = html_escape(&sign_in),
    );
    render_shell(localizer, &title, NavLink::None, &body)
}

/// Render the result page for a completed install. `success` switches
/// the heading between the success and failure i18n keys; `detail` is
/// the raw output (success summary or error chain) shown verbatim in a
/// `<pre>` block after HTML-escaping.
#[must_use]
pub fn render_install_result(localizer: &Localizer, success: bool, detail: &str) -> String {
    render_install_result_with_credentials(localizer, success, detail, &[], None)
}

/// Variant of [`render_install_result`] that also displays one-time
/// credentials generated during the install (e.g. initial admin
/// passwords) above the summary block. Caller MUST have drained the
/// credentials from the job state already; passing an empty slice
/// reverts to the plain result page.
///
/// `rollback_job_id` controls whether a "Roll back this install"
/// button renders below the summary. `Some(id)` posts the rollback
/// to `/install/job/{id}/rollback`; `None` omits the button.
#[must_use]
pub fn render_install_result_with_credentials(
    localizer: &Localizer,
    success: bool,
    detail: &str,
    credentials: &[computeza_driver_native::progress::GeneratedCredential],
    rollback_job_id: Option<&str>,
) -> String {
    let title = localizer.t("ui-install-result-title");
    let outcome = if success {
        localizer.t("ui-install-result-success")
    } else {
        localizer.t("ui-install-result-failed")
    };
    let back = localizer.t("ui-install-result-back");
    let detail_html = html_escape(detail);
    let badge_class = if success {
        "cz-badge cz-badge-ok"
    } else {
        "cz-badge cz-badge-fail"
    };
    let credentials_block = render_credentials_block(localizer, credentials);
    let rollback_block = rollback_job_id
        .map(|id| render_rollback_block(localizer, id))
        .unwrap_or_default();

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p><span class="{badge_class}">{outcome}</span></p>
</section>
{credentials_block}
<section class="cz-section">
<pre class="cz-pre">{detail_html}</pre>
</section>
{rollback_block}
<section class="cz-section">
<a class="cz-btn" href="/install">{back}</a>
</section>"#,
        title = html_escape(&title),
        outcome = html_escape(&outcome),
        back = html_escape(&back),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
}

/// Render the `/admin/secrets` page: one row per secret name with a
/// Rotate button. `names = None` indicates no secrets store is
/// attached -- we render an explanatory card instead of an empty
/// table.
#[must_use]
pub fn render_secrets_index(localizer: &Localizer, names: Option<&[String]>) -> String {
    let title = localizer.t("ui-secrets-title");
    let intro = localizer.t("ui-secrets-intro");
    let store_missing = localizer.t("ui-secrets-store-missing");
    let empty = localizer.t("ui-secrets-empty");
    let col_name = localizer.t("ui-secrets-col-name");
    let col_action = localizer.t("ui-secrets-col-action");
    let rotate = localizer.t("ui-secrets-rotate-button");
    let note = localizer.t("ui-secrets-rotate-note");
    let backup_warning = localizer.t("ui-secrets-backup-warning");

    // Disaster-recovery reminder rendered whenever the secrets store
    // IS attached -- losing the salt / passphrase / ciphertext is
    // irreversible, so the warning gets prominent placement above
    // the listing.
    let backup_block = if names.is_some() {
        format!(
            r#"<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0 0 0.5rem;"><span class="cz-badge cz-badge-warn">Backup required</span></p>
<p class="cz-muted" style="margin: 0; font-size: 0.85rem;">{}</p>
</div>
</section>"#,
            html_escape(&backup_warning)
        )
    } else {
        String::new()
    };

    let body = match names {
        Some([]) => format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{backup_block}
<section class="cz-section">
<div class="cz-card"><p class="cz-card-body" style="margin: 0;">{empty}</p></div>
</section>"#,
            title = html_escape(&title),
            intro = html_escape(&intro),
            empty = html_escape(&empty),
            backup_block = backup_block,
        ),
        None => format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0; color: var(--warn);">{store_missing}</p>
</div>
</section>"#,
            title = html_escape(&title),
            intro = html_escape(&intro),
            store_missing = html_escape(&store_missing),
        ),
        Some(n) => {
            let rows: String = n
                .iter()
                .map(|name| {
                    format!(
                        r#"<tr>
<td><code>{name_html}</code></td>
<td><form method="post" action="/admin/secrets/{name_enc}/rotate" style="margin: 0;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn">{rotate}</button>
</form></td>
</tr>"#,
                        name_html = html_escape(name),
                        name_enc = urlencoding_min(name),
                        rotate = html_escape(&rotate),
                    )
                })
                .collect();

            format!(
                r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{backup_block}
<section class="cz-section">
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{col_name}</th><th>{col_action}</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
<p class="cz-muted" style="margin: 0.85rem 0 0; font-size: 0.85rem;">{note}</p>
</div>
</section>"#,
                title = html_escape(&title),
                intro = html_escape(&intro),
                col_name = html_escape(&col_name),
                col_action = html_escape(&col_action),
                rows = rows,
                note = html_escape(&note),
                backup_block = backup_block,
            )
        }
    };

    render_shell(localizer, &title, NavLink::Secrets, &body)
}

/// Render the post-rotation result page. Shows the freshly-generated
/// value once -- there's no recovery if the operator dismisses this
/// page without copying it out (re-rotating gets a new value, not the
/// missed one).
#[must_use]
pub fn render_secret_rotated(localizer: &Localizer, name: &str, new_value: &str) -> String {
    let title = localizer.t("ui-secrets-rotated-title");
    let warning = localizer.t("ui-install-credentials-warning");
    let name_label = localizer.t("ui-secrets-rotated-name");
    let value_label = localizer.t("ui-secrets-rotated-value");
    let back = localizer.t("ui-secrets-rotated-back");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">{warning}</p>
<dl class="cz-dl">
<dt>{name_label}</dt><dd><code>{name_html}</code></dd>
<dt>{value_label}</dt><dd><code>{value_html}</code></dd>
</dl>
</div>
</section>
<section class="cz-section">
<a class="cz-btn" href="/admin/secrets">{back}</a>
</section>"#,
        title = html_escape(&title),
        warning = html_escape(&warning),
        name_label = html_escape(&name_label),
        value_label = html_escape(&value_label),
        back = html_escape(&back),
        name_html = html_escape(name),
        value_html = html_escape(new_value),
    );

    render_shell(localizer, &title, NavLink::Secrets, &body)
}

/// Render the rollback card on the install result page. Posts to
/// `/install/job/{id}/rollback`, which uninstalls every Done
/// component on the job in reverse dependency order.
fn render_rollback_block(localizer: &Localizer, job_id: &str) -> String {
    let title = localizer.t("ui-install-rollback-title");
    let intro = localizer.t("ui-install-rollback-intro");
    let button = localizer.t("ui-install-rollback-button");
    format!(
        r#"<section class="cz-section" style="max-width: 42rem;">
<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.45);">
<p class="cz-card-body" style="margin: 0 0 0.5rem;"><span class="cz-badge cz-badge-fail">{title}</span></p>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">{intro}</p>
<form method="post" action="/install/job/{job_id}/rollback">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{button}</button>
</form>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        button = html_escape(&button),
        job_id = html_escape(job_id),
    )
}

/// Render the one-time generated-credentials block on the install
/// result page. Returns the empty string when `creds` is empty so the
/// caller can interpolate the result unconditionally.
fn render_credentials_block(
    localizer: &Localizer,
    creds: &[computeza_driver_native::progress::GeneratedCredential],
) -> String {
    if creds.is_empty() {
        return String::new();
    }
    let title = localizer.t("ui-install-credentials-title");
    let warning = localizer.t("ui-install-credentials-warning");
    let comp_h = localizer.t("ui-install-credentials-component");
    let user_h = localizer.t("ui-install-credentials-username");
    let pass_h = localizer.t("ui-install-credentials-password");
    let ref_h = localizer.t("ui-install-credentials-ref");

    let rows: String = creds
        .iter()
        .map(|c| {
            format!(
                r#"<tr>
<td>{component}</td>
<td>{username}</td>
<td><code>{password}</code></td>
<td><code class="cz-muted">{secret_ref}</code></td>
</tr>"#,
                component = html_escape(&c.component),
                username = html_escape(c.username.as_deref().unwrap_or("")),
                password = html_escape(&c.value),
                secret_ref = html_escape(c.secret_ref.as_deref().unwrap_or("")),
            )
        })
        .collect();

    format!(
        r#"<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0 0 0.5rem;"><span class="cz-badge cz-badge-warn">{title}</span></p>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">{warning}</p>
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{comp_h}</th><th>{user_h}</th><th>{pass_h}</th><th>{ref_h}</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</div>
</section>"#,
        title = html_escape(&title),
        warning = html_escape(&warning),
        comp_h = html_escape(&comp_h),
        user_h = html_escape(&user_h),
        pass_h = html_escape(&pass_h),
        ref_h = html_escape(&ref_h),
        rows = rows,
    )
}

/// Render the `/status` page: one row per persisted reconciler
/// observation. `rows = None` means the server is running without a
/// metadata store (no `computeza serve`), and we surface the
/// `ui-status-store-missing` hint instead of an empty table.
#[must_use]
pub fn render_status(localizer: &Localizer, rows: Option<&[StatusRow]>) -> String {
    let title = localizer.t("ui-status-title");
    let intro = localizer.t("ui-status-intro");
    let col_kind = localizer.t("ui-status-col-kind");
    let col_name = localizer.t("ui-status-col-name");
    let col_version = localizer.t("ui-status-col-version");
    let col_observed = localizer.t("ui-status-col-observed");
    let col_state = localizer.t("ui-status-col-state");

    let table_or_hint = match rows {
        None => format!(
            r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0; color: var(--warn);">{}</p></div>"#,
            html_escape(&localizer.t("ui-status-store-missing"))
        ),
        Some([]) => format!(
            r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0;">{}</p></div>"#,
            html_escape(&localizer.t("ui-status-empty"))
        ),
        Some(rs) => {
            let state_ok = localizer.t("ui-status-state-ok");
            let state_failed = localizer.t("ui-status-state-failed");
            let state_unknown = localizer.t("ui-status-state-unknown");
            let body_rows: String = rs
                .iter()
                .map(|r| {
                    let (state_label, badge_cls) = if !r.has_status {
                        (state_unknown.clone(), "cz-badge cz-badge-info")
                    } else if r.last_observe_failed {
                        (state_failed.clone(), "cz-badge cz-badge-fail")
                    } else {
                        (state_ok.clone(), "cz-badge cz-badge-ok")
                    };
                    let version = r.server_version.clone().unwrap_or_else(|| "-".to_string());
                    let observed = r
                        .last_observed_at
                        .clone()
                        .unwrap_or_else(|| "-".to_string());
                    let href = format!(
                        "/resource/{}/{}",
                        urlencoding_min(&r.kind),
                        urlencoding_min(&r.instance_name)
                    );
                    format!(
                        "<tr>\
                         <td class=\"cz-cell-mono cz-cell-dim\">{kind}</td>\
                         <td><a href=\"{href}\" class=\"cz-strong\">{label} / {name}</a></td>\
                         <td class=\"cz-cell-dim\">{version}</td>\
                         <td class=\"cz-cell-mono cz-cell-dim\">{observed}</td>\
                         <td><span class=\"{badge_cls}\">{state_label}</span></td>\
                         </tr>",
                        kind = html_escape(&r.kind),
                        href = href,
                        label = html_escape(&r.component_label),
                        name = html_escape(&r.instance_name),
                        version = html_escape(&version),
                        observed = html_escape(&observed),
                        badge_cls = badge_cls,
                        state_label = html_escape(&state_label),
                    )
                })
                .collect();
            format!(
                r#"<div class="cz-table-wrap">
<table class="cz-table">
<thead><tr>
<th>{col_kind}</th>
<th>{col_name}</th>
<th>{col_version}</th>
<th>{col_observed}</th>
<th>{col_state}</th>
</tr></thead>
<tbody>{body_rows}</tbody>
</table>
</div>"#,
                col_kind = html_escape(&col_kind),
                col_name = html_escape(&col_name),
                col_version = html_escape(&col_version),
                col_observed = html_escape(&col_observed),
                col_state = html_escape(&col_state),
            )
        }
    };

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
{table_or_hint}
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
    );

    render_shell(localizer, &title, NavLink::Status, &body)
}

/// Render the `/state` page: per-kind resource counts pulled live from
/// the SqliteStore. `rows = None` means no store is attached; `Some(&[])`
/// shouldn't happen because the handler always seeds the kind list, but
/// is handled gracefully if it does. The bottom of the page links to
/// the `/api/state/info` JSON endpoint for programmatic callers.
#[must_use]
pub fn render_state_page(localizer: &Localizer, rows: Option<&[StateRow]>) -> String {
    let title = localizer.t("ui-state-title");
    let intro = localizer.t("ui-state-intro");
    let col_kind = localizer.t("ui-state-col-kind");
    let col_count = localizer.t("ui-state-col-count");
    let view_json = localizer.t("ui-state-view-json");

    let table_or_hint = match rows {
        None => format!(
            r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0; color: var(--warn);">{}</p></div>"#,
            html_escape(&localizer.t("ui-state-store-missing"))
        ),
        Some(rs) if rs.iter().all(|r| matches!(&r.count, Ok(0))) => {
            // Store attached, every kind queried fine, but every count
            // is zero -- we surface a "store is empty" hint rather
            // than a wall of zeros.
            format!(
                r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0;">{}</p></div>"#,
                html_escape(&localizer.t("ui-state-store-empty"))
            )
        }
        Some(rs) => {
            let body_rows: String = rs
                .iter()
                .map(|r| {
                    let count_cell = match &r.count {
                        Ok(n) => {
                            let badge = if *n > 0 {
                                "cz-badge cz-badge-ok"
                            } else {
                                "cz-badge cz-badge-info"
                            };
                            format!(r#"<span class="{badge}">{n}</span>"#)
                        }
                        Err(e) => format!(
                            r#"<span class="cz-badge cz-badge-fail">error</span> <span class="cz-cell-dim">{}</span>"#,
                            html_escape(e)
                        ),
                    };
                    format!(
                        "<tr>\
                         <td class=\"cz-cell-mono cz-cell-dim\">{kind}</td>\
                         <td class=\"cz-strong\">{label}</td>\
                         <td>{count_cell}</td>\
                         </tr>",
                        kind = html_escape(&r.kind),
                        label = html_escape(&r.component_label),
                        count_cell = count_cell,
                    )
                })
                .collect();
            format!(
                r#"<div class="cz-table-wrap">
<table class="cz-table">
<thead><tr>
<th>{col_kind}</th>
<th>Component</th>
<th>{col_count}</th>
</tr></thead>
<tbody>{body_rows}</tbody>
</table>
</div>"#,
                col_kind = html_escape(&col_kind),
                col_count = html_escape(&col_count),
            )
        }
    };

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
{table_or_hint}
</section>
<section class="cz-section">
<a class="cz-btn" href="/api/state/info">{view_json}</a>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        view_json = html_escape(&view_json),
    );

    render_shell(localizer, &title, NavLink::State, &body)
}

/// Render the `/resource/{kind}/{name}` page. `stored = Some(_)` means
/// the resource exists and we render the full metadata + spec + status
/// JSON; `stored = None` with `store_missing = false` means a clean 404
/// (resource not in the store); `store_missing = true` means no store is
/// attached to this server at all.
#[must_use]
pub fn render_resource(
    localizer: &Localizer,
    kind: &str,
    name: &str,
    stored: Option<&computeza_state::StoredResource>,
    install_config: Option<&InstallConfig>,
    store_missing: bool,
) -> String {
    let title = localizer.t("ui-resource-title");
    let heading = format!("{} / {}", html_escape(kind), html_escape(name));

    let content = if store_missing {
        format!(
            r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0; color: var(--warn);">{}</p></div>"#,
            html_escape(&localizer.t("ui-resource-store-missing"))
        )
    } else if let Some(sr) = stored {
        let uuid_label = localizer.t("ui-resource-uuid");
        let rev_label = localizer.t("ui-resource-revision");
        let created_label = localizer.t("ui-resource-created-at");
        let updated_label = localizer.t("ui-resource-updated-at");
        let ws_label = localizer.t("ui-resource-workspace");
        let spec_heading = localizer.t("ui-resource-spec-heading");
        let status_heading = localizer.t("ui-resource-status-heading");
        let no_status = localizer.t("ui-resource-no-status");

        let spec_pretty = serde_json::to_string_pretty(&sr.spec).unwrap_or_else(|_| "{}".into());
        let status_block = match &sr.status {
            Some(s) => {
                let pretty = serde_json::to_string_pretty(s).unwrap_or_else(|_| "{}".into());
                format!(r#"<pre class="cz-pre">{}</pre>"#, html_escape(&pretty))
            }
            None => format!(r#"<p class="cz-muted">{}</p>"#, html_escape(&no_status)),
        };
        let workspace = sr
            .key
            .workspace
            .as_deref()
            .map(html_escape)
            .unwrap_or_else(|| "-".to_string());

        // Component slug for the repair / re-install button (kind
        // "postgres-instance" -> slug "postgres"). The button is only
        // rendered when the slug is one we recognize in the unified
        // install dispatcher. The form embeds the persisted
        // InstallConfig (when available) as hidden inputs so the
        // re-install targets the same service name / port / root dir
        // the operator originally chose -- not driver defaults.
        let repair_block = kind
            .strip_suffix("-instance")
            .filter(|s| INSTALL_ORDER.contains(s))
            .map(|slug| {
                let heading = localizer.t("ui-resource-repair-heading");
                let intro = localizer.t("ui-resource-repair-intro");
                let button = localizer.t("ui-resource-repair-button");

                let hidden_inputs = match install_config {
                    Some(c) => {
                        let mut h = String::new();
                        if let Some(v) = &c.version {
                            h.push_str(&format!(
                                r#"<input type="hidden" name="version" value="{}" />"#,
                                html_escape(v)
                            ));
                        }
                        if let Some(p) = c.port {
                            h.push_str(&format!(
                                r#"<input type="hidden" name="port" value="{p}" />"#
                            ));
                        }
                        if let Some(d) = &c.root_dir {
                            h.push_str(&format!(
                                r#"<input type="hidden" name="root_dir" value="{}" />"#,
                                html_escape(d)
                            ));
                        }
                        if let Some(s) = &c.service_name {
                            h.push_str(&format!(
                                r#"<input type="hidden" name="service_name" value="{}" />"#,
                                html_escape(s)
                            ));
                        }
                        h
                    }
                    None => String::new(),
                };

                format!(
                    r#"<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.5rem;">{heading}</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">{intro}</p>
<form method="post" action="/install/{slug}">
<input type="hidden" name="csrf_token" value="" />
<input type="hidden" name="component" value="{slug}" />
{hidden_inputs}
<button type="submit" class="cz-btn">{button}</button>
</form>
</div>
</section>"#,
                    heading = html_escape(&heading),
                    intro = html_escape(&intro),
                    button = html_escape(&button),
                    slug = html_escape(slug),
                    hidden_inputs = hidden_inputs,
                )
            })
            .unwrap_or_default();

        format!(
            r#"<div class="cz-card">
<dl class="cz-dl">
<dt>{uuid_label}</dt><dd>{uuid}</dd>
<dt>{rev_label}</dt><dd>{revision}</dd>
<dt>{created_label}</dt><dd>{created_at}</dd>
<dt>{updated_label}</dt><dd>{updated_at}</dd>
<dt>{ws_label}</dt><dd>{workspace}</dd>
</dl>
</div>
<section class="cz-section">
<h3>{spec_heading}</h3>
<pre class="cz-pre">{spec_html}</pre>
</section>
<section class="cz-section">
<h3>{status_heading}</h3>
{status_block}
</section>
{repair_block}
<section class="cz-section">
<form method="post" action="/resource/{kind_enc}/{name_enc}/delete" onsubmit="return confirm('{confirm}');">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{delete_button}</button>
<p class="cz-muted" style="margin-top: 0.5rem; font-size: 0.85rem;">{confirm_note}</p>
</form>
</section>"#,
            uuid_label = html_escape(&uuid_label),
            rev_label = html_escape(&rev_label),
            created_label = html_escape(&created_label),
            updated_label = html_escape(&updated_label),
            ws_label = html_escape(&ws_label),
            spec_heading = html_escape(&spec_heading),
            status_heading = html_escape(&status_heading),
            uuid = html_escape(&sr.uuid.to_string()),
            revision = sr.revision,
            created_at = html_escape(&sr.created_at.to_rfc3339()),
            updated_at = html_escape(&sr.updated_at.to_rfc3339()),
            spec_html = html_escape(&spec_pretty),
            kind_enc = urlencoding_min(kind),
            name_enc = urlencoding_min(name),
            confirm = html_escape(&localizer.t("ui-resource-delete-confirm")),
            delete_button = html_escape(&localizer.t("ui-resource-delete-button")),
            confirm_note = html_escape(&localizer.t("ui-resource-delete-confirm")),
        )
    } else {
        format!(
            r#"<div class="cz-card"><p class="cz-card-body" style="margin: 0; color: var(--warn);">{}</p></div>"#,
            html_escape(&localizer.t("ui-resource-not-found"))
        )
    };

    let body = format!(
        r#"<section class="cz-hero">
<p class="cz-muted" style="margin: 0 0 0.5rem; text-transform: uppercase; letter-spacing: 0.12em; font-size: 0.75rem;">{title}</p>
<h1>{heading}</h1>
</section>
{content}"#,
        title = html_escape(&title),
    );

    render_shell(localizer, &title, NavLink::Status, &body)
}

/// Render the success page after a delete. Shows a small confirmation
/// and links back to /status.
#[must_use]
pub fn render_resource_deleted(localizer: &Localizer, kind: &str, name: &str) -> String {
    let title = localizer.t("ui-resource-deleted");
    let back = localizer.t("ui-resource-back");
    let heading = format!("{} / {}", html_escape(kind), html_escape(name));

    let body = format!(
        r#"<section class="cz-hero">
<h1>{heading}</h1>
<p><span class="cz-badge cz-badge-ok">{title}</span></p>
</section>
<section class="cz-section">
<a class="cz-btn" href="/status">{back}</a>
</section>"#,
        title = html_escape(&title),
        back = html_escape(&back),
    );

    render_shell(localizer, &title, NavLink::Status, &body)
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
        let html = render_home(&l, StoreSummary::Missing);
        assert!(
            html.contains("Welcome to Computeza"),
            "rendered HTML should contain the localized welcome title; got:\n{html}"
        );
    }

    #[test]
    fn render_home_is_a_complete_html_document() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html lang=\"en\">"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn render_install_shows_form_and_postgres_option() {
        let l = Localizer::english();
        let html = render_install(&l, &[]);
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
    fn render_status_row_links_to_resource_detail() {
        let l = Localizer::english();
        let rows = vec![StatusRow {
            kind: "postgres-instance".into(),
            component_label: "PostgreSQL".into(),
            instance_name: "primary".into(),
            server_version: None,
            last_observed_at: None,
            last_observe_failed: false,
            has_status: true,
        }];
        let html = render_status(&l, Some(&rows));
        assert!(
            html.contains(r#"href="/resource/postgres-instance/primary""#),
            "/status row should link to the resource detail page; got: {html}"
        );
    }

    #[test]
    fn render_resource_store_missing_path() {
        let l = Localizer::english();
        let html = render_resource(&l, "postgres-instance", "primary", None, None, true);
        assert!(html.contains("postgres-instance / primary"));
        assert!(html.contains("needs a metadata store"));
    }

    #[test]
    fn render_resource_not_found_path() {
        let l = Localizer::english();
        let html = render_resource(&l, "kanidm-instance", "missing", None, None, false);
        assert!(html.contains("kanidm-instance / missing"));
        assert!(html.contains("not in the metadata store"));
    }

    #[test]
    fn urlencoding_passes_unreserved_chars_through() {
        assert_eq!(urlencoding_min("postgres-instance"), "postgres-instance");
        assert_eq!(urlencoding_min("primary"), "primary");
        assert_eq!(urlencoding_min("a b"), "a%20b");
        assert_eq!(urlencoding_min("a/b"), "a%2Fb");
    }

    #[test]
    fn render_status_row_marks_failed_observation_with_fail_badge() {
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
        assert!(html.contains("cz-badge-fail"));
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
    fn render_home_landing_links_to_login_and_components() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        // The landing page is the marketing front door; primary CTAs
        // are sign-in and browse-components. The top nav (rendered
        // separately via render_shell) handles the rest of the
        // operator surfaces.
        for href in [r#"href="/login""#, r#"href="/components""#] {
            assert!(
                html.contains(href),
                "landing page should link to {href}; got HTML excerpt:\n{}",
                &html[..html.len().min(2000)]
            );
        }
    }

    #[test]
    fn render_home_landing_renders_all_three_pricing_tiers() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.contains("Community"));
        assert!(html.contains("Pro"));
        assert!(html.contains("Enterprise"));
        assert!(
            html.contains(r#"data-badge="Most popular""#),
            "Pro tier should carry the featured badge"
        );
    }

    #[test]
    fn render_home_landing_carries_all_marketing_sections() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        // Hero
        assert!(html.contains("The Rust-native lakehouse,"));
        assert!(html.contains("Sign in to your console"));
        // Stat strip
        assert!(html.contains("Managed components"));
        // About
        assert!(html.contains("What it is"));
        // Features
        assert!(html.contains("Capabilities"));
        assert!(html.contains("Unified one-click install"));
        // Audiences
        assert!(html.contains("Built for"));
        assert!(html.contains("Platform engineers"));
        // Final CTA
        assert!(html.contains("Run the lakehouse you already understand"));
    }

    #[test]
    fn render_home_has_no_hardcoded_english_strings_outside_attributes() {
        // Sanity check: every <p> and <h*> text node should be a value the
        // localizer produced. We assert by checking that strings the .ftl
        // bundle defines actually appear (positive check).
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.contains("Computeza")); // ui-app-title (in <title>)
        assert!(html.contains("Open lakehouse control plane")); // ui-landing-hero-eyebrow
    }

    #[test]
    fn prereq_banner_is_empty_when_nothing_missing() {
        let l = Localizer::english();
        assert!(render_prerequisite_banner(&l, &[]).is_empty());
    }

    #[test]
    fn prereq_banner_renders_missing_command_with_install_hint() {
        use computeza_driver_native::prerequisites::SystemCommand;
        let l = Localizer::english();
        // Synthesize the command rather than picking one out of
        // SYSTEM_COMMANDS so the test stays decoupled from churn in
        // that table (e.g. the table was emptied of hard host prereqs
        // once cargo got auto-installed and openssl got replaced by
        // rcgen).
        let fake = SystemCommand {
            name: "test-prereq-cmd",
            required_for: "the unit-test purpose",
            install_hint: "apt-get install -y test-prereq-cmd",
        };
        let html = render_prerequisite_banner(&l, &[fake]);
        assert!(
            html.contains("Host command missing"),
            "banner title from ui-prerequisite-banner-title should render"
        );
        assert!(
            html.contains("test-prereq-cmd"),
            "missing command name should render"
        );
        assert!(
            html.contains("apt-get install -y test-prereq-cmd"),
            "install hint should render verbatim"
        );
        assert!(
            html.contains("cz-badge-warn"),
            "banner should use the warn badge style"
        );
    }

    #[test]
    fn missing_prerequisites_returns_empty_for_unknown_name() {
        // Names that aren't in SYSTEM_COMMANDS are filtered out -- even
        // if the host happens not to have them on $PATH -- because we
        // have no install hint to surface for them.
        let result = missing_prerequisites(&["definitely-not-a-real-command-xyzzy"]);
        assert!(result.is_empty());
    }

    #[test]
    fn progress_page_renders_per_component_checklist_when_multi() {
        use computeza_driver_native::progress::{
            ComponentProgress, ComponentState, InstallProgress,
        };
        let l = Localizer::english();
        let p = InstallProgress {
            components: vec![
                ComponentProgress {
                    slug: "postgres".into(),
                    state: ComponentState::Done,
                    summary: Some("ok".into()),
                    error: None,
                },
                ComponentProgress {
                    slug: "openfga".into(),
                    state: ComponentState::Running,
                    summary: None,
                    error: None,
                },
                ComponentProgress {
                    slug: "kanidm".into(),
                    state: ComponentState::Pending,
                    summary: None,
                    error: None,
                },
            ],
            ..Default::default()
        };
        let html = render_install_progress(&l, "job-abc", &p);
        assert!(
            html.contains("Installing Computeza"),
            "multi-component hero must render"
        );
        assert!(html.contains(r#"id="component-postgres""#));
        assert!(html.contains(r#"id="component-openfga""#));
        assert!(html.contains(r#"id="component-kanidm""#));
        assert!(html.contains("Done"));
        assert!(html.contains("Running"));
        assert!(html.contains("Pending"));
    }

    #[test]
    fn progress_page_skips_checklist_when_single_component() {
        let l = Localizer::english();
        let p = InstallProgress::default(); // empty components vec
        let html = render_install_progress(&l, "job-abc", &p);
        assert!(html.contains("Installing component"));
        assert!(!html.contains(r#"id="components""#));
    }

    #[test]
    fn install_hub_renders_a_card_per_component_and_one_global_submit() {
        let l = Localizer::english();
        let html = render_install_hub(&l, &[]);
        assert!(html.contains("Install Computeza"));
        assert!(
            html.contains(r#"name="csrf_token""#),
            "the unified install form must embed a csrf_token input"
        );
        assert!(
            !html.contains("Install in progress"),
            "no active-jobs banner should render when none are passed"
        );
        assert!(
            html.contains(r#"action="/install""#),
            "unified hub form must post to /install"
        );
        for slug in INSTALL_ORDER {
            assert!(
                html.contains(&format!(r#"name="{slug}__service_name""#)),
                "card for {slug} must collect a service_name field"
            );
            assert!(
                html.contains(&format!(r#"name="{slug}__port""#)),
                "card for {slug} must collect a port field"
            );
        }
        let install_button_occurrences = html.matches("Install all components").count();
        assert_eq!(
            install_button_occurrences, 1,
            "there must be exactly one global Install button"
        );
        assert!(html.contains("Identity and access"));
        assert!(html.contains("Configurable in v0.1+"));
    }

    #[test]
    fn install_hub_renders_active_job_banner_with_resume_link() {
        let l = Localizer::english();
        let active = vec![ActiveJob {
            id: "abc-123".into(),
            running_slug: Some("kanidm".into()),
            components_done: 3,
            components_total: 10,
        }];
        let html = render_install_hub(&l, &active);
        assert!(html.contains("Install in progress"));
        assert!(html.contains(r#"href="/install/job/abc-123""#));
        assert!(html.contains("3/10 components"));
        assert!(html.contains("kanidm currently running"));
    }

    #[test]
    fn install_order_only_lists_available_components() {
        for slug in INSTALL_ORDER {
            let entry = COMPONENTS
                .iter()
                .find(|c| c.slug == *slug)
                .unwrap_or_else(|| panic!("INSTALL_ORDER references unknown slug {slug:?}"));
            assert!(
                entry.available,
                "INSTALL_ORDER entry {slug:?} must be marked available=true in COMPONENTS"
            );
        }
    }

    #[test]
    fn build_unified_config_parses_per_slug_fields() {
        let mut form = HashMap::new();
        form.insert("postgres__port".into(), "5433".into());
        form.insert(
            "postgres__service_name".into(),
            "computeza-postgres-18".into(),
        );
        form.insert("postgres__root_dir".into(), "/srv/pg18".into());
        form.insert("postgres__version".into(), "18.3-1".into());
        // kanidm fields should NOT bleed into the postgres config.
        form.insert("kanidm__port".into(), "9999".into());

        let cfg = build_unified_config(&form, "postgres").expect("postgres config");
        assert_eq!(cfg.port, Some(5433));
        assert_eq!(cfg.service_name.as_deref(), Some("computeza-postgres-18"));
        assert_eq!(cfg.root_dir.as_deref(), Some("/srv/pg18"));
        assert_eq!(cfg.version.as_deref(), Some("18.3-1"));
    }

    #[test]
    fn build_unified_config_rejects_bad_service_name() {
        let mut form = HashMap::new();
        form.insert(
            "postgres__service_name".into(),
            "bad name with spaces".into(),
        );
        let err = build_unified_config(&form, "postgres").unwrap_err();
        assert!(err.contains("service name"));
    }

    #[test]
    fn build_unified_config_blank_fields_resolve_to_driver_defaults() {
        let form: HashMap<String, String> = HashMap::new();
        let cfg = build_unified_config(&form, "kanidm").expect("kanidm config");
        assert!(cfg.port.is_none());
        assert!(cfg.service_name.is_none());
        assert!(cfg.root_dir.is_none());
        assert!(cfg.version.is_none());
    }

    // ---- generate_random_password ----

    #[test]
    fn generate_random_password_is_24_hex_chars_and_unique() {
        let a = generate_random_password();
        let b = generate_random_password();
        assert_eq!(a.len(), 24, "expected 24-char hex string, got {a:?}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two random passwords must differ (entropy=96 bits)");
    }

    // ---- render_credentials_block ----

    #[test]
    fn render_credentials_block_is_empty_when_no_credentials() {
        let l = Localizer::english();
        assert!(render_credentials_block(&l, &[]).is_empty());
    }

    #[test]
    fn render_credentials_block_includes_warning_and_each_row() {
        use computeza_driver_native::progress::GeneratedCredential;
        let l = Localizer::english();
        let creds = vec![
            GeneratedCredential {
                component: "postgres".into(),
                label: "superuser password".into(),
                value: "deadbeef1234".into(),
                username: Some("postgres".into()),
                secret_ref: Some("postgres/admin-password".into()),
            },
            GeneratedCredential {
                component: "kanidm".into(),
                label: "initial admin password".into(),
                value: "cafebabe5678".into(),
                username: Some("admin".into()),
                secret_ref: Some("kanidm/admin-password".into()),
            },
        ];
        let html = render_credentials_block(&l, &creds);
        assert!(html.contains("Generated credentials"));
        assert!(html.contains("Copy these values"));
        assert!(html.contains("postgres"));
        assert!(html.contains("deadbeef1234"));
        assert!(html.contains("kanidm"));
        assert!(html.contains("cafebabe5678"));
        assert!(html.contains("postgres/admin-password"));
    }

    // ---- render_install_result_with_credentials ----

    #[test]
    fn install_result_with_credentials_omits_rollback_when_id_is_none() {
        let l = Localizer::english();
        let html = render_install_result_with_credentials(&l, true, "ok", &[], None);
        assert!(!html.contains("Roll back this install"));
    }

    #[test]
    fn install_result_with_credentials_renders_rollback_when_id_provided() {
        let l = Localizer::english();
        let html = render_install_result_with_credentials(&l, true, "ok", &[], Some("abc-123"));
        assert!(html.contains("Roll back this install"));
        assert!(html.contains(r#"action="/install/job/abc-123/rollback""#));
    }

    // ---- render_secrets_index ----

    #[test]
    fn render_secrets_index_renders_store_missing_when_none() {
        let l = Localizer::english();
        let html = render_secrets_index(&l, None);
        assert!(html.contains("No secrets store is attached"));
        assert!(
            !html.contains("Backup required"),
            "backup card should NOT render when no store is attached"
        );
    }

    #[test]
    fn render_secrets_index_renders_empty_state_when_no_entries() {
        let l = Localizer::english();
        let html = render_secrets_index(&l, Some(&[]));
        assert!(html.contains("no secrets are stored yet"));
        assert!(
            html.contains("Backup required"),
            "backup-required card should appear even on an empty store -- the operator may add entries soon"
        );
    }

    #[test]
    fn render_secrets_index_renders_rotate_button_per_row() {
        let l = Localizer::english();
        let names = vec![
            "postgres/admin-password".to_string(),
            "kanidm/admin-password".to_string(),
        ];
        let html = render_secrets_index(&l, Some(&names));
        assert!(html.contains("postgres/admin-password"));
        assert!(html.contains("kanidm/admin-password"));
        // Two rotate forms, one per row.
        let rotate_count = html.matches("/admin/secrets/").count();
        assert!(
            rotate_count >= 2,
            "expected at least 2 rotate form actions, got {rotate_count}"
        );
        assert!(html.contains("Backup required"));
    }

    // ---- render_secret_rotated ----

    #[test]
    fn render_secret_rotated_shows_name_and_value_and_warning() {
        let l = Localizer::english();
        let html = render_secret_rotated(&l, "postgres/admin-password", "newvalue1234");
        assert!(html.contains("postgres/admin-password"));
        assert!(html.contains("newvalue1234"));
        assert!(html.contains("Copy these values"));
        assert!(html.contains(r#"href="/admin/secrets""#));
    }

    // ---- render_resource repair-button hidden inputs ----

    #[test]
    fn render_resource_repair_form_embeds_persisted_install_config() {
        use computeza_state::StoredResource;
        let l = Localizer::english();
        // The StoredResource shape only needs the spec field to be valid
        // JSON for render_resource to format the dl/uuid/etc; we don't
        // exercise that path here.
        let stored = StoredResource {
            key: ResourceKey::cluster_scoped("postgres-instance", "local"),
            uuid: uuid::Uuid::nil(),
            revision: 1,
            spec: serde_json::json!({}),
            status: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let config = InstallConfig {
            version: Some("18.3-1".into()),
            port: Some(5433),
            root_dir: Some("/srv/pg-staging".into()),
            service_name: Some("computeza-postgres-staging".into()),
        };
        let html = render_resource(
            &l,
            "postgres-instance",
            "local",
            Some(&stored),
            Some(&config),
            false,
        );
        assert!(html.contains(r#"action="/install/postgres""#));
        assert!(html.contains(r#"name="component" value="postgres""#));
        assert!(html.contains(r#"name="version" value="18.3-1""#));
        assert!(html.contains(r#"name="port" value="5433""#));
        assert!(html.contains(r#"name="root_dir" value="/srv/pg-staging""#));
        assert!(html.contains(r#"name="service_name" value="computeza-postgres-staging""#));
    }

    #[test]
    fn render_resource_repair_form_renders_with_no_persisted_config() {
        use computeza_state::StoredResource;
        let l = Localizer::english();
        let stored = StoredResource {
            key: ResourceKey::cluster_scoped("postgres-instance", "local"),
            uuid: uuid::Uuid::nil(),
            revision: 1,
            spec: serde_json::json!({}),
            status: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let html = render_resource(&l, "postgres-instance", "local", Some(&stored), None, false);
        // The form still renders with the component slug, but with no
        // version / port / root_dir / service_name overrides.
        assert!(html.contains(r#"action="/install/postgres""#));
        assert!(html.contains(r#"name="component" value="postgres""#));
        assert!(!html.contains(r#"name="version""#));
        assert!(!html.contains(r#"name="port""#));
        assert!(!html.contains(r#"name="root_dir""#));
        assert!(!html.contains(r#"name="service_name""#));
    }

    // ---- InstallConfig serde round-trip ----

    #[test]
    fn install_config_roundtrips_through_json_with_partial_fields() {
        let original = InstallConfig {
            version: Some("18.3-1".into()),
            port: Some(5433),
            root_dir: None,
            service_name: Some("computeza-postgres-staging".into()),
        };
        let v = serde_json::to_value(&original).expect("serialize");
        // None fields skip-serialize so blank input forms don't round-
        // trip null into the metadata store -- they're absent.
        assert!(
            v.get("root_dir").is_none(),
            "None fields must skip serialize"
        );
        let back: InstallConfig = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back.version, original.version);
        assert_eq!(back.port, original.port);
        assert_eq!(back.root_dir, original.root_dir);
        assert_eq!(back.service_name, original.service_name);
    }

    #[test]
    fn install_config_default_is_all_none() {
        let c = InstallConfig::default();
        assert!(c.version.is_none());
        assert!(c.port.is_none());
        assert!(c.root_dir.is_none());
        assert!(c.service_name.is_none());
    }

    // ---- apply_uninstall_config_overrides (Linux-only) ----

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_uninstall_config_overrides_honors_service_name_and_root_dir() {
        let config = InstallConfig {
            service_name: Some("my-postgres".into()),
            root_dir: Some("/srv/pg".into()),
            ..Default::default()
        };
        let mut unit = "computeza-postgres.service".to_string();
        let mut root = std::path::PathBuf::from("/var/lib/computeza/postgres");
        apply_uninstall_config_overrides(&config, &mut unit, &mut root);
        assert_eq!(unit, "my-postgres.service");
        assert_eq!(root, std::path::PathBuf::from("/srv/pg"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_uninstall_config_overrides_keeps_defaults_when_unset() {
        let config = InstallConfig::default();
        let mut unit = "computeza-postgres.service".to_string();
        let mut root = std::path::PathBuf::from("/var/lib/computeza/postgres");
        apply_uninstall_config_overrides(&config, &mut unit, &mut root);
        assert_eq!(unit, "computeza-postgres.service");
        assert_eq!(
            root,
            std::path::PathBuf::from("/var/lib/computeza/postgres")
        );
    }

    // ---- dispatch_install / dispatch_uninstall_with_config: unknown slugs ----

    #[tokio::test]
    async fn dispatch_install_unknown_slug_returns_error() {
        let progress = ProgressHandle::noop();
        let result =
            dispatch_install("not-a-real-slug", &progress, &InstallConfig::default()).await;
        let err = result.expect_err("unknown slug should be Err");
        assert!(err.contains("unknown component slug"));
    }

    #[tokio::test]
    async fn dispatch_uninstall_with_config_unknown_slug_returns_error() {
        let result =
            dispatch_uninstall_with_config("not-a-real-slug", &InstallConfig::default()).await;
        let err = result.expect_err("unknown slug should be Err");
        // On Linux the error names the slug; on non-Linux it's the
        // platform-not-supported message. Both are valid errors.
        assert!(!err.is_empty());
    }

    // ---- InstallConfig round-trip through save_install_config / load_install_config ----

    #[tokio::test]
    async fn install_config_persistence_roundtrips_through_sqlite_store() {
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .expect("open in-memory sqlite store");
        let config = InstallConfig {
            version: Some("18.3-1".into()),
            port: Some(5433),
            root_dir: Some("/srv/pg".into()),
            service_name: Some("computeza-postgres-staging".into()),
        };
        save_install_config(&store, "postgres", &config)
            .await
            .expect("save_install_config");
        let back = load_install_config(&store, "postgres")
            .await
            .expect("load_install_config returned None");
        assert_eq!(back.version, config.version);
        assert_eq!(back.port, config.port);
        assert_eq!(back.root_dir, config.root_dir);
        assert_eq!(back.service_name, config.service_name);

        // Upsert: a second save with the same slug should not error.
        let updated = InstallConfig {
            port: Some(5434),
            ..config.clone()
        };
        save_install_config(&store, "postgres", &updated)
            .await
            .expect("upsert save");
        let back2 = load_install_config(&store, "postgres").await.unwrap();
        assert_eq!(back2.port, Some(5434));

        // Delete leaves load returning None.
        delete_install_config(&store, "postgres").await;
        assert!(load_install_config(&store, "postgres").await.is_none());
    }

    #[tokio::test]
    async fn load_install_config_returns_none_when_absent() {
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .unwrap();
        assert!(load_install_config(&store, "postgres").await.is_none());
    }

    // ---- hydrate_postgres_password ----

    #[tokio::test]
    async fn hydrate_postgres_password_no_secrets_store_leaves_password_unchanged() {
        let mut spec = computeza_reconciler_postgres::PostgresSpec {
            endpoint: computeza_reconciler_postgres::ServerEndpoint {
                host: "127.0.0.1".into(),
                port: 5432,
                superuser: "postgres".into(),
                sslmode: None,
            },
            superuser_password: secrecy::SecretString::from(String::new()),
            superuser_password_ref: Some("postgres/admin-password".into()),
            databases: Vec::new(),
            prune: false,
        };
        hydrate_postgres_password(&mut spec, None).await;
        use secrecy::ExposeSecret;
        assert_eq!(spec.superuser_password.expose_secret(), "");
    }

    #[tokio::test]
    async fn finalize_managed_install_persists_spec_and_install_config() {
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .unwrap();
        let config = InstallConfig {
            service_name: Some("computeza-postgres-staging".into()),
            port: Some(5433),
            root_dir: Some("/srv/pg".into()),
            version: None,
        };
        let spec = serde_json::json!({"endpoint": {"host": "127.0.0.1", "port": 5433}});

        let state = Arc::new(StdMutex::new(InstallProgress::default()));
        let progress = ProgressHandle::new(state.clone());

        let summary = finalize_managed_install_after_success(
            "postgres",
            &config,
            &spec,
            "ok".to_string(),
            Some(&store),
            None,
            &progress,
        )
        .await;
        assert!(summary.contains("Registered as postgres-instance/local"));

        // Spec persisted.
        let spec_row = store
            .load(&ResourceKey::cluster_scoped("postgres-instance", "local"))
            .await
            .unwrap()
            .expect("spec row missing");
        assert_eq!(spec_row.spec, spec);

        // Install-config persisted.
        let cfg = load_install_config(&store, "postgres").await.unwrap();
        assert_eq!(cfg.service_name, config.service_name);
        assert_eq!(cfg.port, config.port);

        // Credentials pushed (postgres is in COMPONENTS_WITH_ADMIN_CREDENTIAL).
        let creds = progress.drain_credentials();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].component, "postgres");
        assert_eq!(creds[0].value.len(), 24);
    }

    #[tokio::test]
    async fn finalize_managed_install_skips_credentials_for_components_without_one() {
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .unwrap();
        let state = Arc::new(StdMutex::new(InstallProgress::default()));
        let progress = ProgressHandle::new(state.clone());

        // garage is not in COMPONENTS_WITH_ADMIN_CREDENTIAL; no
        // credential should be generated, but the spec + install-
        // config should still be persisted.
        let _ = finalize_managed_install_after_success(
            "garage",
            &InstallConfig::default(),
            &serde_json::json!({"endpoint": {"base_url": "http://127.0.0.1:3903"}}),
            "ok".to_string(),
            Some(&store),
            None,
            &progress,
        )
        .await;
        assert!(progress.drain_credentials().is_empty());
        assert!(load_install_config(&store, "garage").await.is_some());
    }

    #[tokio::test]
    async fn teardown_managed_uninstall_handles_unknown_slug_cleanly() {
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .unwrap();
        let result = teardown_managed_uninstall("not-a-real-slug", Some(&store), None).await;
        assert!(
            result.is_err(),
            "unknown slug should propagate the dispatch error"
        );
    }

    #[tokio::test]
    async fn teardown_managed_uninstall_drops_install_config_row() {
        // Pre-populate an install-config and ensure teardown removes it
        // even on dispatch failure (operator may have manually deleted
        // the service; we still want the metadata clean).
        let store = computeza_state::SqliteStore::open(":memory:")
            .await
            .unwrap();
        save_install_config(&store, "postgres", &InstallConfig::default())
            .await
            .unwrap();
        assert!(load_install_config(&store, "postgres").await.is_some());

        // Dispatch will fail on a Windows/non-Linux test runner since
        // dispatch_uninstall_with_config has a non-Linux Err branch;
        // teardown_managed_uninstall continues to drop the metadata
        // either way. On Linux runners the dispatch attempts a real
        // teardown which will Err because no service exists -- same
        // cleanup behavior.
        let _ = teardown_managed_uninstall("postgres", Some(&store), None).await;
        assert!(
            load_install_config(&store, "postgres").await.is_none(),
            "teardown must drop install-config regardless of dispatch result"
        );
    }

    #[test]
    fn render_audit_renders_store_missing_when_none() {
        let l = Localizer::english();
        let html = render_audit(&l, None, None);
        assert!(html.contains("No audit log is attached"));
        assert!(!html.contains("Verifying key"));
    }

    #[test]
    fn render_audit_renders_empty_state() {
        let l = Localizer::english();
        let html = render_audit(&l, Some(&[]), None);
        assert!(html.contains("No audit events recorded yet"));
    }

    #[test]
    fn render_audit_renders_events_newest_first_with_columns() {
        use computeza_audit::{Action, AuditEvent, AuditEventBody};
        let l = Localizer::english();
        let event_a = AuditEvent {
            body: AuditEventBody {
                id: uuid::Uuid::nil(),
                seq: 1,
                timestamp: chrono::DateTime::parse_from_rfc3339("2026-05-12T10:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                prev_digest: String::new(),
                actor: "admin".into(),
                action: Action::Authn,
                resource: None,
                detail: serde_json::json!({"username": "admin"}),
            },
            signature: "sig-base64".into(),
        };
        let html = render_audit(&l, Some(&[event_a]), Some("pubkey-base64"));
        assert!(html.contains("admin"));
        assert!(html.contains("Authn"));
        assert!(html.contains("2026-05-12T10:00:00"));
        assert!(html.contains("Verifying key"));
        assert!(html.contains("pubkey-base64"));
    }

    #[tokio::test]
    async fn hydrate_postgres_password_no_ref_leaves_password_unchanged() {
        // No secrets-store lookup needed -- the spec doesn't carry a ref.
        let mut spec = computeza_reconciler_postgres::PostgresSpec {
            endpoint: computeza_reconciler_postgres::ServerEndpoint {
                host: "127.0.0.1".into(),
                port: 5432,
                superuser: "postgres".into(),
                sslmode: None,
            },
            superuser_password: secrecy::SecretString::from("preset".to_string()),
            superuser_password_ref: None,
            databases: Vec::new(),
            prune: false,
        };
        hydrate_postgres_password(&mut spec, None).await;
        use secrecy::ExposeSecret;
        assert_eq!(spec.superuser_password.expose_secret(), "preset");
    }
}
