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
    http::{header, HeaderMap, HeaderValue, StatusCode},
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
    /// Loaded license envelope. `None` means community mode (no
    /// envelope activated yet). The lock is taken briefly per
    /// activation / deactivation; reads are uncontended in steady
    /// state.
    pub license: Arc<tokio::sync::RwLock<Option<computeza_license::License>>>,
    /// On-disk path where the envelope is persisted (typically
    /// `<state_db_parent>/license.json`). `None` on the smoke-test
    /// surface where there is no state directory; activation /
    /// deactivation is a no-op in that mode.
    pub license_path: Option<Arc<std::path::PathBuf>>,
    /// EU AI Act model-card registry. `None` for the smoke-test
    /// surface; the binary opens one at
    /// `<state_db_parent>/model-cards.jsonl` at boot.
    pub model_cards: Option<computeza_compliance::ModelCardRegistry>,
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
            license: Arc::new(tokio::sync::RwLock::new(None)),
            license_path: None,
            model_cards: None,
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
            license: Arc::new(tokio::sync::RwLock::new(None)),
            license_path: None,
            model_cards: None,
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

    /// Set the on-disk path the activation / deactivation handlers
    /// persist to. Typically `<state_db_parent>/license.json`.
    #[must_use]
    pub fn with_license_path(mut self, path: std::path::PathBuf) -> Self {
        self.license_path = Some(Arc::new(path));
        self
    }

    /// Attach the EU AI Act model-card registry. Typically opened
    /// at `<state_db_parent>/model-cards.jsonl` at boot.
    #[must_use]
    pub fn with_model_cards(mut self, registry: computeza_compliance::ModelCardRegistry) -> Self {
        self.model_cards = Some(registry);
        self
    }

    /// Seed the loaded license. Used at boot after
    /// [`computeza_license::load_license_file`] returns `Some`.
    pub async fn set_license(&self, license: Option<computeza_license::License>) {
        *self.license.write().await = license;
    }

    /// Snapshot the current license status against `now`. Returns
    /// [`computeza_license::LicenseStatus::None`] when no envelope is
    /// active (community mode). Used by the kill-switch middleware
    /// and the banner injector.
    pub async fn license_status(&self) -> computeza_license::LicenseStatus {
        let guard = self.license.read().await;
        match guard.as_ref() {
            None => computeza_license::LicenseStatus::None,
            Some(lic) => lic.status(Some(&computeza_license::trusted_root()), chrono::Utc::now()),
        }
    }

    /// Count active operator accounts. Used by the seat-cap check on
    /// `/admin/operators` create.
    pub async fn operator_count(&self) -> usize {
        match &self.operators {
            Some(o) => o.list().await.len(),
            None => 0,
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

/// License kill-switch middleware -- runs after CSRF (so we know
/// the request is genuine before we decide whether to honor it).
///
/// Reads [`AppState::license_status`]; when the status does not allow
/// mutations (Expired / Invalid / NotYetValid), every non-public POST
/// request returns 403 with a renewal page. GET requests flow through
/// unchanged so the operator can still inspect state, sign out, and
/// activate a fresh license. Two routes are always allowed through
/// regardless of status: `/admin/license/activate` (so the operator
/// can install a new envelope) and `/logout` (so they can step out
/// of the console without dead-ending). The data plane is not
/// touched -- services keep running; only the control plane goes
/// read-only.
async fn license_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::Method;

    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let path = request.uri().path().to_string();
    if auth::is_public_path(&path) {
        return next.run(request).await;
    }
    // Always-allowed POST routes: license activation/deactivation
    // (the operator must be able to fix an expired install) + logout
    // (escape hatch).
    if matches!(
        path.as_str(),
        "/admin/license/activate" | "/admin/license/deactivate" | "/logout"
    ) {
        return next.run(request).await;
    }
    let status = state.license_status().await;
    if status.allow_mutations() {
        return next.run(request).await;
    }
    tracing::warn!(
        method = %request.method(),
        path = %path,
        status = ?status,
        "license enforcement: rejecting mutating request in read-only mode (license status forbids mutations)"
    );
    (
        StatusCode::FORBIDDEN,
        Html(render_license_blocked(status, &path)),
    )
        .into_response()
}

fn render_license_blocked(status: computeza_license::LicenseStatus, path: &str) -> String {
    let title = match status {
        computeza_license::LicenseStatus::Expired { days_since_expiry } => {
            format!("License expired {days_since_expiry} day(s) ago")
        }
        computeza_license::LicenseStatus::NotYetValid { days_until_valid } => {
            format!("License not yet valid (effective in {days_until_valid} day(s))")
        }
        computeza_license::LicenseStatus::Invalid(reason) => {
            format!("License invalid ({reason})")
        }
        _ => "License check failed".into(),
    };
    format!(
        "<!DOCTYPE html><html><head><title>{title}</title>\
         <link rel=\"stylesheet\" href=\"/static/computeza.css\" /></head>\
         <body style=\"font-family:sans-serif;padding:2rem;max-width:48rem;margin:0 auto;\">\
         <h1>{title}</h1>\
         <p>The mutating request to <code>{path}</code> was rejected because \
         the active Computeza license does not currently permit mutations. \
         The data plane keeps running; only the control plane is read-only \
         until a valid license is activated.</p>\
         <p>To resolve: visit <a href=\"/admin/license\">/admin/license</a> \
         to activate a fresh envelope, or contact your reseller / Computeza \
         sales for a renewal.</p>\
         <p><a href=\"/admin/license\">Activate or replace license</a> &middot; \
         <a href=\"/\">Back to the landing page</a></p>\
         </body></html>",
        title = html_escape(&title),
        path = html_escape(path),
    )
}

/// Build the axum router with an `AppState` attached. Every handler that
/// needs the store extracts it via `State<AppState>`.
pub fn router_with_state(state: AppState) -> Router {
    let auth_layer = axum::middleware::from_fn_with_state(state.clone(), auth_middleware);
    let permission_layer =
        axum::middleware::from_fn_with_state(state.clone(), permission_middleware);
    let csrf_layer = axum::middleware::from_fn_with_state(state.clone(), csrf_middleware);
    let license_layer = axum::middleware::from_fn_with_state(state.clone(), license_middleware);
    Router::new()
        .route("/", get(home_handler))
        .route("/components", get(components_handler))
        .route("/install-guide", get(install_guide_handler))
        .route("/studio", get(studio_handler))
        .route("/studio/sql/execute", post(studio_sql_execute_handler))
        // Workspace file browser (SQL / Python / text snippets the
        // operator authors inside Studio). CRUD + import/export +
        // .cptz archive build/parse. State lives in
        // computeza-state's studio_files table.
        .route("/studio/files/new", post(studio_file_create_handler))
        .route("/studio/files/{id}/save", post(studio_file_save_handler))
        .route("/studio/files/{id}/delete", post(studio_file_delete_handler))
        .route("/studio/files/{id}/duplicate", post(studio_file_duplicate_handler))
        .route("/studio/files/{id}/rename", post(studio_file_rename_handler))
        .route("/studio/files/{id}/export", get(studio_file_export_handler))
        .route("/studio/files/import", post(studio_file_import_handler))
        .route("/studio/files/export-archive", get(studio_files_export_archive_handler))
        .route(
            "/studio/api/completions",
            get(studio_completions_handler),
        )
        .route(
            "/studio/bootstrap",
            get(studio_bootstrap_form_handler).post(studio_bootstrap_submit_handler),
        )
        .route("/studio/bootstrap/reset", post(studio_bootstrap_reset_handler))
        // Iceberg-REST catalog drill-down (phase 1.5):
        //   /studio/catalog/{warehouse}                       -> namespace list
        //   /studio/catalog/{warehouse}/{namespace}           -> table list
        //   /studio/catalog/{warehouse}/{namespace}/{table}   -> table detail
        // All three hit Lakekeeper's /catalog/v1/* surface and
        // surface response bodies verbatim on non-2xx so URL-pattern
        // drift across Lakekeeper releases is debuggable from the
        // page itself.
        .route(
            "/studio/catalog/{warehouse}",
            get(studio_catalog_warehouse_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/namespaces/create",
            post(studio_catalog_namespace_create_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/wire-trino",
            post(studio_catalog_wire_trino_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/{namespace}",
            get(studio_catalog_namespace_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/{namespace}/delete",
            post(studio_catalog_namespace_delete_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/{namespace}/tables/create",
            post(studio_catalog_table_create_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/{namespace}/{table}",
            get(studio_catalog_table_handler),
        )
        .route(
            "/studio/catalog/{warehouse}/{namespace}/{table}/delete",
            post(studio_catalog_table_delete_handler),
        )
        .route(
            "/install",
            get(install_hub_handler).post(install_all_handler),
        )
        .route(
            "/install/{slug}/retry-bootstrap",
            post(retry_bootstrap_handler),
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
        .route(
            "/install/credentials.json/{id}",
            get(install_credentials_json_handler),
        )
        .route("/install/job/{id}/rollback", post(install_rollback_handler))
        .route("/api/install/job/{id}", get(install_job_api_handler))
        .route("/admin/secrets", get(secrets_index_handler))
        .route(
            "/admin/secrets/setup/generate-passphrase",
            post(secrets_setup_generate_handler),
        )
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
        .route("/admin/tenants", get(admin_tenants_handler))
        .route(
            "/admin/branding",
            get(admin_branding_handler).post(admin_branding_save_handler),
        )
        .route("/admin/license", get(admin_license_handler))
        .route(
            "/admin/license/activate",
            post(admin_license_activate_handler),
        )
        .route(
            "/admin/license/deactivate",
            post(admin_license_deactivate_handler),
        )
        .route("/admin/pq-status", get(admin_pq_status_handler))
        .route("/compliance/eu-ai-act", get(compliance_eu_ai_act_handler))
        .route(
            "/compliance/models",
            get(compliance_models_list_handler).post(compliance_models_create_handler),
        )
        .route(
            "/compliance/models/{id}",
            get(compliance_models_detail_handler),
        )
        .route(
            "/compliance/models/{id}/delete",
            post(compliance_models_delete_handler),
        )
        .route("/api/license/status", get(api_license_status_handler))
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
        .layer(license_layer)
        .layer(csrf_layer)
        .layer(permission_layer)
        .layer(auth_layer)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ============================================================
// Studio surface (phase 1: catalog browser + SQL editor)
// ============================================================
//
// The /studio page exposes two side-by-side pieces of the
// installed lakehouse:
//
//   1. Catalog browser (left) -- lists Iceberg namespaces + tables
//      by hitting the local Lakekeeper REST catalog. Each table
//      surfaces a "Pre-fill SELECT *" link that bounces back to
//      /studio with `?sql=...` so the editor on the right loads
//      with a starter query. No client-side JS required.
//
//   2. SQL editor (right) -- POSTs the query to
//      /studio/sql/execute which forwards to Databend's HTTP
//      query handler and renders the response as an HTML table
//      below the editor.
//
// Phase 1 deliberately uses a textarea (not Monaco) and full-page
// form submits (no HTMX/JS). Each piece is independently useful and
// the architecture stays inside the existing SSR pattern. Monaco /
// inline result streaming / catalog tree-view are follow-up work
// once the roundtrip is proven.
//
// Endpoint discovery: both Lakekeeper and Databend get their URLs
// from the metadata store's `{slug}-instance/local` row spec --
// same pattern reconcile_tick uses. No hardcoded 127.0.0.1:* here;
// if the operator changed the port at install time the studio
// follows.

/// Minimal shape of `LakekeeperSpec` / `DatabendSpec` we need to
/// pull the endpoint URL out. Defining inline avoids dragging the
/// full reconciler crates into ui-server just for one field.
#[derive(serde::Deserialize)]
struct StudioEndpointSpec {
    endpoint: StudioEndpointUrl,
}

#[derive(serde::Deserialize)]
struct StudioEndpointUrl {
    base_url: String,
}

/// Look up the base URL of the locally-installed Lakekeeper.
/// Returns `None` when no `lakekeeper-instance/local` row exists in
/// the store (component not installed) or the spec doesn't
/// deserialize (operator hand-edited the row to something
/// incompatible -- corner case worth handling without panicking).
async fn discover_lakekeeper_endpoint(
    store: Option<&computeza_state::SqliteStore>,
) -> Option<String> {
    use computeza_state::Store;
    let store = store?;
    let rows = store.list("lakekeeper-instance", None).await.ok()?;
    let sr = rows.into_iter().next()?;
    let spec: StudioEndpointSpec = serde_json::from_value(sr.spec).ok()?;
    Some(spec.endpoint.base_url)
}

/// Cap on how many recent queries we retain per studio. Older
/// entries get dropped when the list exceeds this. 20 is a
/// balance between "useful enough to recover yesterday's work" and
/// "small enough that a single resource row stays well under the
/// 1MB SQLite-blob soft ceiling" -- 20 entries * ~2KB SQL each
/// leaves plenty of headroom.
const STUDIO_HISTORY_CAP: usize = 20;

/// One row in the studio SQL history. Persisted as JSON inside
/// a single resource row (`studio-history/default`) so we don't
/// need a new SQLite table for v0.0.x. Multi-studio history
/// (one row per studio name) lands when the studio selector
/// stops being hard-coded to "default".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StudioHistoryEntry {
    /// The query text, verbatim. v0.0.x stores plaintext; the
    /// deferred secrets-redaction layer (AGENTS.md "Deferred work")
    /// will replace this with a sanitised variant once it ships.
    sql: String,
    /// UTC timestamp when the query was submitted.
    executed_at: chrono::DateTime<chrono::Utc>,
    /// True iff Databend returned a result set (not an error).
    ok: bool,
    /// Number of rows returned. `None` for errored queries or
    /// queries that don't produce rows (DDL, INSERT without
    /// RETURNING).
    row_count: Option<usize>,
}

/// Wrapper struct for the studio-history resource spec. The
/// `entries` vec is newest-first so the UI can iterate in display
/// order without re-sorting.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct StudioHistory {
    #[serde(default)]
    entries: Vec<StudioHistoryEntry>,
}

/// Read the studio SQL history from the metadata store. Returns
/// an empty history when no row exists yet (first query of a fresh
/// install) or when the store isn't attached.
async fn load_studio_history(
    store: Option<&computeza_state::SqliteStore>,
) -> StudioHistory {
    use computeza_state::Store;
    let Some(store) = store else {
        return StudioHistory::default();
    };
    let key = computeza_state::ResourceKey::cluster_scoped("studio-history", "default");
    match store.load(&key).await {
        Ok(Some(stored)) => serde_json::from_value(stored.spec).unwrap_or_default(),
        _ => StudioHistory::default(),
    }
}

/// Prepend a new history entry and persist. Best-effort: a save
/// failure logs but doesn't block the SQL roundtrip from
/// returning to the operator. Caps the list at
/// `STUDIO_HISTORY_CAP` entries (drops oldest).
async fn record_studio_history(
    store: Option<&computeza_state::SqliteStore>,
    entry: StudioHistoryEntry,
) {
    use computeza_state::Store;
    let Some(store) = store else {
        return;
    };
    let key = computeza_state::ResourceKey::cluster_scoped("studio-history", "default");
    let existing = match store.load(&key).await {
        Ok(Some(s)) => Some(s),
        _ => None,
    };
    let mut history: StudioHistory = existing
        .as_ref()
        .and_then(|s| serde_json::from_value(s.spec.clone()).ok())
        .unwrap_or_default();
    history.entries.insert(0, entry);
    history.entries.truncate(STUDIO_HISTORY_CAP);
    let value = match serde_json::to_value(&history) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not serialise studio history; skipping save");
            return;
        }
    };
    let expected_revision = existing.as_ref().map(|s| s.revision);
    if let Err(e) = store.save(&key, &value, expected_revision).await {
        tracing::warn!(
            error = %e,
            "could not persist studio history; the SQL result is still rendered, only the history list won't include this query"
        );
    }
}

/// Trino's HTTP coordinator URL discovered from the metadata store.
/// Studio's SQL editor routes every SQL query here; the bootstrap
/// step writes Iceberg-REST catalog properties files against this
/// endpoint via the driver helper. Returns None if no trino-instance
/// row is registered (Trino not installed yet).
async fn discover_trino_endpoint(
    store: Option<&computeza_state::SqliteStore>,
) -> Option<String> {
    use computeza_state::Store;
    let store = store?;
    let rows = store.list("trino-instance", None).await.ok()?;
    let sr = rows.into_iter().next()?;
    let spec: StudioEndpointSpec = serde_json::from_value(sr.spec).ok()?;
    Some(spec.endpoint.base_url)
}

/// Sail's HTTP-ish base URL for liveness purposes. Spark Connect
/// itself is gRPC, but the same host:port answers TCP probes, so we
/// return the http://host:port form that the existing reconciler /
/// status pages already render. The Studio Python executor reads
/// the raw host + port from this URL to build the `sc://` URI.
async fn discover_sail_endpoint(
    store: Option<&computeza_state::SqliteStore>,
) -> Option<String> {
    use computeza_state::Store;
    let store = store?;
    let rows = store.list("sail-instance", None).await.ok()?;
    let sr = rows.into_iter().next()?;
    let spec: StudioEndpointSpec = serde_json::from_value(sr.spec).ok()?;
    Some(spec.endpoint.base_url)
}

/// What we learned from talking to the local Lakekeeper. Four
/// states map to four different UX paths -- "not reachable" wants
/// /status; "no warehouses" wants the bootstrap docs; "has
/// warehouses" wants the browse-namespaces flow; "unexpected" is
/// the diagnostic escape hatch when Lakekeeper returns 2xx but the
/// body doesn't match the shape we expect (so URL-pattern drift
/// across releases is debuggable from the page itself).
#[derive(Debug, Clone)]
enum LakekeeperState {
    /// `/management/v1/warehouse` or the project-list call failed
    /// (Lakekeeper unreachable, 5xx, malformed JSON).
    Unreachable,
    /// Lakekeeper is up but has no warehouses configured. v0.0.x
    /// + v0.1 auto-bootstrap covers this; manual recovery is the
    /// /studio/bootstrap form.
    NoWarehouses,
    /// At least one warehouse exists. The strings are the warehouse
    /// NAMES that the drill-down routes use as URL prefixes when
    /// hitting `/catalog/v1/{warehouse}/...`.
    HasWarehouses(Vec<String>),
    /// Lakekeeper responded with 2xx but the response body didn't
    /// parse into the warehouse-list shape we expected. Carries
    /// the raw body so the operator (or future iterations) can see
    /// what Lakekeeper actually sent. Surfaces in the catalog pane
    /// as a diagnostic block. The String is read via the Debug
    /// impl, not destructured -- silence the compiler hint.
    #[allow(dead_code)]
    UnexpectedShape(String),
}

/// Probe Lakekeeper for its warehouse list. Lakekeeper's
/// `/management/v1/warehouse` endpoint is project-scoped: without
/// a project-id query param it returns nothing useful. The right
/// shape is "list projects, then list warehouses per project,
/// aggregate names".
///
/// Falls through to `UnexpectedShape` if Lakekeeper returns 2xx
/// but no warehouse names can be extracted -- the raw body lands
/// in the catalog pane so URL-pattern + response-shape drift is
/// debuggable from /studio directly.
/// Lakekeeper's `/catalog/v1/{prefix}/*` endpoints require the
/// warehouse UUID, not the human-friendly name. The /studio drill-
/// down routes carry the name in their URL (so operators see
/// `default` in the breadcrumb, not a 36-char UUID). This helper
/// resolves a name to its UUID via a three-tier strategy:
///
///   1. If the input already looks like a UUID, pass through.
///   2. Look up `lakekeeper/default-warehouse-id` from vault
///      (populated by a prior successful bootstrap).
///   3. Auto-discover via the Iceberg REST config endpoint:
///      `GET /catalog/v1/config?warehouse=<name>` returns
///      `{"overrides": {"prefix": "<uuid>"}}`. This is the
///      standard Iceberg-REST way to resolve a warehouse name to
///      its opaque prefix; engines do this on every connection.
///      The discovered prefix is cached back to the vault.
///
/// If all three fail, returns the input verbatim and Lakekeeper's
/// `WarehouseIdIsNotUUID` error surfaces in the drill-down page.
async fn resolve_warehouse_id_or_pass(
    name_or_uuid: &str,
    base_url: Option<&str>,
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> String {
    // Tier 1: literal UUID.
    if name_or_uuid.len() == 36
        && name_or_uuid.chars().filter(|c| *c == '-').count() == 4
        && name_or_uuid
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return name_or_uuid.to_string();
    }
    // Tier 2: vault cache (set by bootstrap or prior auto-discover).
    //
    // PER-NAME key so multiple warehouses don't trample each other's
    // cache. The legacy `lakekeeper/default-warehouse-id` singleton
    // key is read as a fallback for backward compat (set by older
    // bootstrap runs); new writes go to the per-name key.
    if let Some(s) = secrets {
        use secrecy::ExposeSecret;
        let by_name_key = format!("lakekeeper/warehouse-id-by-name/{name_or_uuid}");
        if let Ok(Some(v)) = s.get(&by_name_key).await {
            let id = v.expose_secret().to_string();
            if !id.trim().is_empty() {
                return id;
            }
        }
        // Legacy singleton fallback: only honor it when the
        // requested name is the historic "default" warehouse,
        // otherwise we'd hand back a different warehouse's UUID
        // and confuse Lakekeeper (the exact bug per-name keys fix).
        if name_or_uuid == "default" {
            if let Ok(Some(v)) = s.get("lakekeeper/default-warehouse-id").await {
                let id = v.expose_secret().to_string();
                if !id.trim().is_empty() {
                    return id;
                }
            }
        }
    }
    // Tier 3: auto-discover via Iceberg REST `/catalog/v1/config`.
    // Standard Iceberg-REST behaviour: pass the warehouse name as
    // a query param, get the prefix back in the `overrides` block.
    if let Some(url) = base_url {
        if let Some(prefix) = try_discover_via_config(name_or_uuid, url, secrets).await {
            return prefix;
        }
    }
    name_or_uuid.to_string()
}

/// Force-fresh resolve. Skips the vault cache and hits /v1/config.
///
/// Originally added to fix "wire-databend used a stale UUID", but
/// turned out to be the wrong fix: when Lakekeeper has multiple
/// warehouses with the same name (e.g. one orphaned by failed
/// recovery), /v1/config picks one of them, which may not be the
/// one drill-down successfully recovered. Vault is now authoritative
/// because drill-down auto-recovery writes the working UUID there.
///
/// Kept for callers that explicitly want to bust the cache (e.g.
/// post-uninstall-reinstall flows where vault and Lakekeeper have
/// diverged on purpose).
#[allow(dead_code)]
async fn resolve_warehouse_id_fresh(
    name_or_uuid: &str,
    base_url: &str,
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> String {
    // If the input is already shaped like a UUID, trust the caller
    // -- they passed an authoritative ID, no point re-discovering.
    if name_or_uuid.len() == 36
        && name_or_uuid.chars().filter(|c| *c == '-').count() == 4
        && name_or_uuid
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return name_or_uuid.to_string();
    }
    // Always hit /catalog/v1/config. Falls through to the input name
    // unchanged on 404 / network error so the caller sees a clear
    // engine-side error rather than a silent succeed-with-stale-data.
    if let Some(prefix) = try_discover_via_config(name_or_uuid, base_url, secrets).await {
        return prefix;
    }
    name_or_uuid.to_string()
}

/// Hit /catalog/v1/config?warehouse=<name>; parse the
/// `overrides.prefix`; cache to vault. Returns None on any failure
/// (404 NoSuchWarehouseException, wrong shape, network error,
/// missing prefix field). Used by both the resolver and the
/// recovery path.
/// Nil/default project UUID. Lakekeeper's `LAKEKEEPER__ENABLE_DEFAULT_PROJECT`
/// (default true) makes the nil project the implicit landing zone for
/// warehouses created without explicit project context. Every
/// Iceberg-REST request out of Studio sends this as the
/// `x-project-id` header so the read path matches the write path
/// (bootstrap's `POST /management/v1/warehouse` also sends it).
const LAKEKEEPER_NIL_PROJECT: &str = "00000000-0000-0000-0000-000000000000";

async fn try_discover_via_config(
    name: &str,
    base_url: &str,
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let cfg_url = format!(
        "{}/catalog/v1/config?warehouse={}",
        base_url.trim_end_matches('/'),
        url_encode(name)
    );
    // x-project-id keeps every Iceberg-REST request inside the nil
    // project so we see the warehouses our bootstrap created there.
    // Without the header Lakekeeper may default-route to a different
    // project context and 404 even when the warehouse exists.
    let resp = client
        .get(&cfg_url)
        .header("x-project-id", LAKEKEEPER_NIL_PROJECT)
        .send()
        .await
        .ok()?;
    let status = resp.status();
    let text = resp.text().await.ok()?;
    tracing::info!(
        url = %cfg_url,
        status = status.as_u16(),
        body = %text,
        "try_discover_via_config: /catalog/v1/config probe"
    );
    if !status.is_success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let prefix = v
        .get("overrides")
        .and_then(|o| o.get("prefix"))
        .or_else(|| v.get("prefix"))
        .and_then(|p| p.as_str())?
        .to_string();
    if prefix.trim().is_empty() {
        return None;
    }
    if let Some(s) = secrets {
        // Per-name cache so multiple warehouses don't share a slot.
        // Also write the legacy singleton key when the resolved
        // name is "default" so older code paths reading that key
        // keep working.
        let by_name_key = format!("lakekeeper/warehouse-id-by-name/{name}");
        let _ = s.put(&by_name_key, &prefix).await;
        if name == "default" {
            let _ = s.put("lakekeeper/default-warehouse-id", &prefix).await;
        }
    }
    tracing::info!(
        warehouse_name = %name,
        prefix = %prefix,
        "try_discover_via_config: discovered + cached"
    );
    Some(prefix)
}

/// Auto-recovery: called when the resolver couldn't find a UUID
/// AND a subsequent catalog-REST call returned a "warehouse not
/// found" error. If the vault has Garage credentials (= bootstrap
/// state from a previous successful run), re-fire the Lakekeeper
/// bootstrap to re-create the warehouse, then retry discovery.
///
/// Returns Some(uuid) on successful recovery, None otherwise.
/// Logs verbosely so the operator can see what happened in the
/// server console.
async fn try_recover_missing_warehouse(
    name: &str,
    base_url: &str,
    secrets: &computeza_secrets::SecretsStore,
    store: Option<&computeza_state::SqliteStore>,
) -> Option<String> {
    use secrecy::ExposeSecret;
    // Pre-flight: bootstrap state must exist in vault. If it
    // doesn't, this is a fresh install that hasn't been bootstrapped
    // yet -- not our job to autostart.
    let key_id = secrets.get("garage/lakekeeper-key-id").await.ok().flatten()?;
    let secret = secrets.get("garage/lakekeeper-secret").await.ok().flatten()?;
    let bucket = secrets
        .get("garage/lakekeeper-bucket")
        .await
        .ok()
        .flatten()
        .map(|v| v.expose_secret().to_string())
        .unwrap_or_else(|| "lakekeeper-default".to_string());
    let garage_endpoint = discover_garage_endpoint(store)
        .await
        .map(|u| u.replace(":3903", ":3900"))
        .unwrap_or_else(|| "http://127.0.0.1:3900".to_string());

    tracing::warn!(
        warehouse_name = %name,
        "Lakekeeper says warehouse missing but vault has bootstrap state; auto-recovering"
    );
    let form = StudioBootstrapForm {
        project_name: "computeza-default".to_string(),
        warehouse_name: name.to_string(),
        s3_endpoint: garage_endpoint,
        s3_region: "garage".to_string(),
        s3_bucket: bucket,
        s3_access_key: key_id.expose_secret().to_string(),
        s3_secret_access_key: secret.expose_secret().to_string(),
    };
    match run_lakekeeper_bootstrap(base_url, &form).await {
        Ok(ok) => {
            if let Some(id) = &ok.warehouse_id {
                let _ = secrets.put("lakekeeper/default-warehouse-id", id).await;
                tracing::info!(prefix = %id, "auto-recover: bootstrap re-ran successfully; warehouse UUID now in vault");
                return Some(id.clone());
            }
            // Bootstrap succeeded but didn't capture UUID -- retry
            // discovery via /catalog/v1/config to pick it up.
            try_discover_via_config(name, base_url, Some(secrets)).await
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto-recover: bootstrap re-run failed");
            None
        }
    }
}

/// Wrapper around `probe_lakekeeper` that falls back to the vault
/// when Lakekeeper's warehouse-list returns empty. Reason: in some
/// Lakekeeper configurations the warehouse-list endpoint is
/// gated by auth, project scope, or other constraints that don't
/// apply to the data-plane / catalog REST endpoints we use for
/// drill-down. The bootstrap step (auto or manual via /studio/bootstrap)
/// persists the warehouse name into `lakekeeper/default-warehouse-name`
/// after a confirmed success. If we have that value, the warehouse
/// definitely exists -- present it.
///
/// On NoWarehouses + vault entry present -> HasWarehouses([name]).
/// On any other probe outcome (Unreachable / HasWarehouses /
/// UnexpectedShape) -> pass through unchanged.
async fn probe_lakekeeper_with_vault_fallback(
    base_url: &str,
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> LakekeeperState {
    let probe = probe_lakekeeper(base_url).await;
    // Fall back only on the "list returned empty" case. Unreachable
    // / HasWarehouses / UnexpectedShape pass through unchanged.
    if !matches!(probe, LakekeeperState::NoWarehouses) {
        return probe;
    }

    use secrecy::ExposeSecret;
    let secrets = match secrets {
        Some(s) => s,
        None => return probe,
    };

    // Tier 1: vault has the warehouse name from a previous successful
    // bootstrap. Cheapest path. Hit if the new (post-fix)
    // /studio/bootstrap submit handler ran since the last DB reset.
    if let Ok(Some(v)) = secrets.get("lakekeeper/default-warehouse-name").await {
        let name = v.expose_secret().to_string();
        if !name.trim().is_empty() {
            tracing::info!(
                warehouse = %name,
                "probe_lakekeeper: list empty; vault has warehouse name -- using"
            );
            return LakekeeperState::HasWarehouses(vec![name]);
        }
    }

    // Tier 2: management API said empty AND vault has no warehouse
    // name, but Garage credentials ARE in vault -- which means the
    // operator DID run a bootstrap at some point, we just lost track
    // of the warehouse name (pre-fix where the submit handler didn't
    // persist the name). Probe the Iceberg catalog REST directly for
    // common defaults; that endpoint is what engines actually use, so
    // it's authoritative regardless of management-API quirks.
    let garage_provisioned = matches!(
        secrets.get("garage/lakekeeper-key-id").await,
        Ok(Some(_))
    );
    if !garage_provisioned {
        // No prior bootstrap detected; the "no warehouses" verdict is
        // honest.
        return probe;
    }
    tracing::info!(
        "probe_lakekeeper: management API empty but garage creds exist in vault; \
         attempting catalog-REST probe for known warehouse-name defaults"
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return probe,
    };
    let base = base_url.trim_end_matches('/');
    // Order from most-likely (the bootstrap form's default) to less-
    // likely. First 2xx wins.
    for candidate in ["default", "computeza-default", "lakekeeper-default"] {
        let url = format!("{base}/catalog/v1/{candidate}/namespaces");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(
                    warehouse = %candidate,
                    url = %url,
                    "probe_lakekeeper: catalog REST confirmed warehouse exists; backfilling vault"
                );
                // Backfill the vault so future renders short-circuit
                // at tier 1.
                let _ = secrets
                    .put("lakekeeper/default-warehouse-name", candidate)
                    .await;
                return LakekeeperState::HasWarehouses(vec![candidate.to_string()]);
            }
            Ok(resp) => {
                tracing::info!(
                    warehouse = %candidate,
                    status = resp.status().as_u16(),
                    "probe_lakekeeper: catalog REST said no for this name"
                );
            }
            Err(e) => {
                tracing::info!(
                    warehouse = %candidate,
                    error = %e,
                    "probe_lakekeeper: catalog REST request failed for this name"
                );
            }
        }
    }
    probe
}

async fn probe_lakekeeper(base_url: &str) -> LakekeeperState {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return LakekeeperState::Unreachable,
    };
    let base = base_url.trim_end_matches('/');

    // Build the endpoint list defensively. We ALWAYS try the
    // unscoped warehouse endpoint -- that's the one the reconciler
    // already validates as part of /status, so it's the most-likely-
    // working URL. If we can also list projects, we additionally
    // try project-scoped endpoints. Only if EVERY endpoint we try
    // returns non-2xx (or doesn't respond) do we conclude
    // Unreachable.
    let mut endpoints: Vec<String> = vec![format!("{base}/management/v1/warehouse")];

    // Best-effort project enrichment. If /management/v1/project
    // fails or returns a shape we don't recognize, just skip the
    // project-scoped variants -- the unscoped endpoint is still
    // tried below.
    let proj_url = format!("{base}/management/v1/project");
    if let Ok(proj_resp) = client.get(&proj_url).send().await {
        if proj_resp.status().is_success() {
            if let Ok(proj_text) = proj_resp.text().await {
                if let Ok(proj_v) = serde_json::from_str::<serde_json::Value>(&proj_text) {
                    let project_ids: Vec<String> = proj_v
                        .get("projects")
                        .or_else(|| proj_v.get("data"))
                        .and_then(|x| x.as_array())
                        .cloned()
                        .or_else(|| proj_v.as_array().cloned())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|p| {
                            p.get("project-id")
                                .or_else(|| p.get("projectId"))
                                .or_else(|| p.get("id"))
                                .and_then(|x| x.as_str())
                                .map(str::to_string)
                        })
                        .collect();
                    for pid in &project_ids {
                        endpoints.push(format!(
                            "{base}/management/v1/warehouse?project-id={pid}"
                        ));
                    }
                }
            }
        }
    }

    let mut all_names: Vec<String> = Vec::new();
    let mut last_raw_body: Option<String> = None;
    let mut any_2xx = false;

    for url in &endpoints {
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(url = %url, error = %e, "probe_lakekeeper: request failed");
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            tracing::info!(url = %url, status = status.as_u16(), "probe_lakekeeper: non-2xx");
            continue;
        }
        any_2xx = true;
        let text = match resp.text().await {
            Ok(t) => t,
            Err(_) => continue,
        };
        tracing::info!(url = %url, status = status.as_u16(), body = %text, "probe_lakekeeper: response");
        last_raw_body = Some(text.clone());
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let warehouses = v
            .get("warehouses")
            .or_else(|| v.get("data"))
            .and_then(|x| x.as_array())
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default();
        for w in &warehouses {
            let name = w
                .get("name")
                .or_else(|| w.get("warehouse-name"))
                .or_else(|| w.get("warehouseName"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
                .or_else(|| {
                    w.get("warehouse-id")
                        .or_else(|| w.get("warehouseId"))
                        .or_else(|| w.get("id"))
                        .and_then(|n| n.as_str())
                        .map(str::to_string)
                });
            if let Some(n) = name {
                if !all_names.contains(&n) {
                    all_names.push(n);
                }
            }
        }
    }

    if !any_2xx {
        // Every warehouse-list endpoint we tried failed -- treat
        // this as Lakekeeper-side unreachability. The reconciler's
        // /status will say more about why.
        return LakekeeperState::Unreachable;
    }
    if !all_names.is_empty() {
        return LakekeeperState::HasWarehouses(all_names);
    }
    // 2xx but no names parsed. If body looks honestly empty,
    // NoWarehouses. Otherwise UnexpectedShape with the raw body
    // for paste-back debugging.
    let looks_empty = match &last_raw_body {
        None => true,
        Some(b) => {
            let t = b.trim();
            t.is_empty()
                || t == "[]"
                || t == "{}"
                || t.contains("\"warehouses\":[]")
                || t.contains("\"data\":[]")
        }
    };
    if looks_empty {
        LakekeeperState::NoWarehouses
    } else {
        LakekeeperState::UnexpectedShape(format!(
            "Lakekeeper returned 2xx from one or more warehouse-list endpoints, but the response \
             body didn't match the expected `{{\"warehouses\": [...]}}` shape with `name` / \
             `warehouse-name` fields per entry.\n\n\
             Tried endpoints (in order):\n  - {endpoints}\n\n\
             Last response body:\n{body}\n\n\
             To fix: adjust the field-name candidates in probe_lakekeeper() in \
             crates/computeza-ui-server/src/lib.rs to match the actual shape above.",
            endpoints = endpoints.join("\n  - "),
            body = last_raw_body.unwrap_or_default(),
        ))
    }
}

// ============================================================
// Iceberg-REST catalog drill-down (phase 1.5)
// ============================================================
//
// Three routes under /studio/catalog/* let the operator navigate
// the catalog the way they navigate a filesystem:
//   warehouse -> namespaces -> tables -> table detail.
//
// All three hit Lakekeeper's /catalog/v1/* Iceberg-REST surface.
// URL prefix patterns drift across Lakekeeper releases (some use
// warehouse name, some UUID, some configurable via /v1/config) so
// each handler surfaces the full Lakekeeper response body verbatim
// on non-2xx for debuggability.
//
// Row preview is deferred to phase 1.6 -- it requires Databend ->
// Lakekeeper Iceberg catalog wiring (a Databend `[[catalog]]` block
// pointing at Lakekeeper's REST endpoint) which doesn't exist yet.

/// Fetch JSON from Lakekeeper's catalog REST. Returns a tuple of
/// (status, parsed-json-or-error-text). Caller decides what shapes
/// it expects; this just handles the HTTP + parsing boilerplate.
async fn get_lakekeeper_catalog_json(
    base_url: &str,
    path: &str,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let resp = client
        .get(&url)
        .header("x-project-id", LAKEKEEPER_NIL_PROJECT)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(could not read body: {e})"));
    if !status.is_success() {
        return Err(format!(
            "GET {url} returned {}:\n{text}\n\nIf Lakekeeper uses a different URL pattern for this resource (warehouse UUID vs name, /catalog/v1 vs /iceberg/v1, etc.), iterate via this verbose error.",
            status.as_u16()
        ));
    }
    serde_json::from_str(&text)
        .map_err(|e| format!("GET {url} body did not parse as JSON: {e}\nbody: {text}"))
}

async fn studio_catalog_warehouse_handler(
    State(state): State<AppState>,
    axum::extract::Path(warehouse): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<FlashQuery>,
) -> Html<String> {
    let l = Localizer::english();
    let focus = SidebarFocus {
        warehouse: Some(&warehouse),
        namespace: None,
        table: None,
    };
    let sidebar = build_studio_full_sidebar(&state, focus).await;
    let lakekeeper = discover_lakekeeper_endpoint(state.store.as_deref()).await;
    let Some(base_url) = lakekeeper else {
        return Html(render_studio_drilldown_error(
            &l,
            &[(&warehouse, &format!("/studio/catalog/{warehouse}"))],
            "Lakekeeper is not registered in the metadata store. Install Lakekeeper from /install first.",
            &sidebar,
        ));
    };
    // Resolve human-friendly name -> UUID (Lakekeeper's catalog
    // REST refuses non-UUID prefixes with `WarehouseIdIsNotUUID`).
    let mut wh_id =
        resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let mut result =
        get_lakekeeper_catalog_json(&base_url, &format!("/catalog/v1/{}/namespaces", url_encode(&wh_id)))
            .await;
    // Auto-recovery: if Lakekeeper says the warehouse doesn't exist
    // but we have bootstrap state in vault, re-fire the bootstrap
    // and retry. Triggered by NoSuchWarehouse or WarehouseIdIsNotUUID
    // errors -- both mean "this prefix doesn't resolve to a warehouse".
    let mut recovery_report: Option<String> = None;
    if let (Err(e), Some(secrets)) = (&result, state.secrets.as_deref()) {
        let lkstr = e.to_lowercase();
        if lkstr.contains("nosuchwarehouse") || lkstr.contains("warehouseidisnotuuid") {
            tracing::warn!(
                warehouse = %warehouse,
                "studio drill-down: state mismatch detected; running auto-recovery"
            );
            match try_recover_missing_warehouse(
                &warehouse,
                &base_url,
                secrets,
                state.store.as_deref(),
            )
            .await
            {
                Some(recovered_id) => {
                    tracing::info!(
                        warehouse = %warehouse,
                        prefix = %recovered_id,
                        "studio drill-down: auto-recovery succeeded; retrying namespaces call"
                    );
                    wh_id = recovered_id;
                    result = get_lakekeeper_catalog_json(
                        &base_url,
                        &format!("/catalog/v1/{}/namespaces", url_encode(&wh_id)),
                    )
                    .await;
                    recovery_report = Some(format!(
                        "Auto-recover: Lakekeeper had lost the warehouse `{warehouse}`. Re-bootstrapped from vault state (warehouse UUID is now {wh_id}). If you're seeing this banner repeatedly, it means Lakekeeper's persistence is getting wiped between requests -- check that postgres is up + that you're not uninstalling Lakekeeper between page loads."
                    ));
                }
                None => {
                    recovery_report = Some(format!(
                        "Auto-recover attempted but failed. Lakekeeper says the warehouse `{warehouse}` doesn't exist, and re-running the bootstrap from vault state didn't fix it. Most likely cause: the Garage credentials in vault are stale (e.g. you rotated the Garage key without re-bootstrapping). Visit /studio/bootstrap to manually provision -- the form is pre-filled from vault and you can update the access-key/secret if they've changed.",
                    ));
                }
            }
        }
    }
    // Recovery banner takes precedence over a flash error from a
    // redirected POST (recovery is the more urgent issue). Otherwise
    // surface the ?err=... query so the operator sees why the create
    // POST failed. ?wired=... is the success path of "Connect to SQL"
    // -- routed through the same renderer but styled as success.
    let final_html = render_studio_namespace_list(
        &l,
        &warehouse,
        result,
        recovery_report.as_deref(),
        q.err.as_deref(),
        q.wired.as_deref(),
        &sidebar,
    );
    Html(final_html)
}

async fn studio_catalog_namespace_handler(
    State(state): State<AppState>,
    axum::extract::Path((warehouse, namespace)): axum::extract::Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<FlashQuery>,
) -> Html<String> {
    let l = Localizer::english();
    let focus = SidebarFocus {
        warehouse: Some(&warehouse),
        namespace: Some(&namespace),
        table: None,
    };
    let sidebar = build_studio_full_sidebar(&state, focus).await;
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return Html(render_studio_drilldown_error(
            &l,
            &[
                (&warehouse, &format!("/studio/catalog/{warehouse}")),
                (&namespace, &format!("/studio/catalog/{warehouse}/{namespace}")),
            ],
            "Lakekeeper is not registered.",
            &sidebar,
        ));
    };
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    // Multi-level namespaces ("finance.raw") become %1F-separated
    // (unit-separator) in the Iceberg REST URL spec. Single-level
    // is the common case in v0.0.x; we still encode for safety.
    let ns_encoded = namespace.split('.').collect::<Vec<_>>().join("%1F");
    let path = format!(
        "/catalog/v1/{}/namespaces/{}/tables",
        url_encode(&wh_id),
        ns_encoded
    );
    let result = get_lakekeeper_catalog_json(&base_url, &path).await;
    Html(render_studio_table_list(
        &l,
        &warehouse,
        &namespace,
        result,
        q.err.as_deref(),
        &sidebar,
    ))
}

/// Shared query shape for catalog GET handlers that may receive a
/// flash message from a redirected POST: `err` for failures, `wired`
/// for success of the "Connect to SQL" action. Stays at the request
/// boundary -- renderers take the unwrapped &str so they don't have
/// to know about query parsing.
#[derive(serde::Deserialize, Default)]
struct FlashQuery {
    err: Option<String>,
    wired: Option<String>,
    #[serde(default)]
    flash: Option<String>,
}

async fn studio_catalog_table_handler(
    State(state): State<AppState>,
    axum::extract::Path((warehouse, namespace, table)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> Html<String> {
    let l = Localizer::english();
    let focus = SidebarFocus {
        warehouse: Some(&warehouse),
        namespace: Some(&namespace),
        table: Some(&table),
    };
    let sidebar = build_studio_full_sidebar(&state, focus).await;
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return Html(render_studio_drilldown_error(
            &l,
            &[
                (&warehouse, &format!("/studio/catalog/{warehouse}")),
                (&namespace, &format!("/studio/catalog/{warehouse}/{namespace}")),
                (&table, &format!("/studio/catalog/{warehouse}/{namespace}/{table}")),
            ],
            "Lakekeeper is not registered.",
            &sidebar,
        ));
    };
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let ns_encoded = namespace.split('.').collect::<Vec<_>>().join("%1F");
    let path = format!(
        "/catalog/v1/{}/namespaces/{}/tables/{}",
        url_encode(&wh_id),
        ns_encoded,
        url_encode(&table)
    );
    let result = get_lakekeeper_catalog_json(&base_url, &path).await;
    Html(render_studio_table_detail(
        &l, &warehouse, &namespace, &table, result, &sidebar,
    ))
}

/// Shared POST/DELETE helper for the catalog mutate endpoints.
/// Builds a request, fires it against Lakekeeper, returns the
/// response body verbatim on non-2xx so URL-pattern / schema-shape
/// drift across Lakekeeper releases is debuggable from the
/// resulting flash message.
async fn lakekeeper_catalog_mutate(
    base_url: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut req = client
        .request(method.clone(), &url)
        .header("x-project-id", LAKEKEEPER_NIL_PROJECT);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(could not read body: {e})"));
    // Treat 200/201/204 as success. 409 (conflict / already exists)
    // also counts as "the state we want exists" -- callers passing a
    // create request are happy if the resource already exists.
    if status.is_success() || status.as_u16() == 409 {
        return Ok(());
    }
    Err(format!(
        "{method} {url} returned {}:\n{text}",
        status.as_u16()
    ))
}

#[derive(serde::Deserialize)]
struct CreateNamespaceForm {
    name: String,
}

/// POST /studio/catalog/{warehouse}/wire-trino
///
/// Re-fires `CREATE CATALOG` against Trino for an already-bootstrapped
/// warehouse, using credentials cached in vault. Lets the operator
/// wire SQL access on demand without re-entering keys -- useful when:
///   - Lakekeeper was bootstrapped before the auto-wire shipped
///   - Trino was installed AFTER the original bootstrap
///   - The CREATE CATALOG silently failed during bootstrap (network
///     glitch, missing meta, version mismatch) and needs a retry
///
/// Redirects back to the warehouse page with `?err=...` on failure or
/// `?wired=...` on success so the operator sees the outcome inline.
async fn studio_catalog_wire_trino_handler(
    State(state): State<AppState>,
    axum::extract::Path(warehouse): axum::extract::Path<String>,
) -> axum::response::Redirect {
    use secrecy::ExposeSecret;
    let base_redirect = format!("/studio/catalog/{warehouse}");
    let err_redirect = |msg: &str| {
        axum::response::Redirect::to(&format!("{base_redirect}?err={}", url_encode(msg)))
    };
    let Some(secrets) = state.secrets.as_deref() else {
        return err_redirect("Vault is not configured; cannot read cached credentials.");
    };
    let Some(lakekeeper_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return err_redirect("Lakekeeper endpoint is not registered.");
    };
    // Pull the Garage credentials + bucket from vault. These were
    // persisted by the bootstrap form (see studio_bootstrap_submit_handler).
    let Some(key_id) = secrets.get("garage/lakekeeper-key-id").await.ok().flatten() else {
        return err_redirect(
            "Garage access key ID not in vault. Re-run /studio/bootstrap to populate credentials.",
        );
    };
    let Some(secret_key) = secrets.get("garage/lakekeeper-secret").await.ok().flatten() else {
        return err_redirect(
            "Garage secret access key not in vault. Re-run /studio/bootstrap to populate credentials.",
        );
    };
    let bucket = secrets
        .get("garage/lakekeeper-bucket")
        .await
        .ok()
        .flatten()
        .map(|v| v.expose_secret().to_string())
        .unwrap_or_else(|| "lakekeeper-default".to_string());
    let garage_endpoint = discover_garage_endpoint(state.store.as_deref())
        .await
        .map(|u| u.replace(":3903", ":3900"))
        .unwrap_or_else(|| "http://127.0.0.1:3900".to_string());
    let form = StudioBootstrapForm {
        project_name: "computeza-default".to_string(),
        warehouse_name: warehouse.clone(),
        s3_endpoint: garage_endpoint,
        s3_region: "garage".to_string(),
        s3_bucket: bucket,
        s3_access_key: key_id.expose_secret().to_string(),
        s3_secret_access_key: secret_key.expose_secret().to_string(),
    };
    // First attempt -- uses the vault-cached UUID (set by drill-down
    // auto-recovery, so usually fresh + reachable).
    let attempt = wire_trino_iceberg_catalog(
        state.store.as_deref(),
        state.secrets.as_deref(),
        &lakekeeper_url,
        &form,
    )
    .await;

    // If the first attempt 404s with NoSuchWarehouse, the cached
    // UUID is pointing at a deleted warehouse. Same auto-recovery
    // path drill-down uses: re-run bootstrap, capture the new UUID,
    // retry. Single retry only -- if recovery itself fails, surface
    // the original error so the operator has full context.
    let final_outcome = match attempt {
        TrinoWiringOutcome::Failed { ref reason }
            if reason.to_lowercase().contains("nosuchwarehouse")
                || reason.to_lowercase().contains("warehouseidisnotuuid") =>
        {
            tracing::warn!(
                warehouse = %warehouse,
                first_error = %reason,
                "wire-trino: NoSuchWarehouse on first attempt; running auto-recovery"
            );
            match try_recover_missing_warehouse(
                &warehouse,
                &lakekeeper_url,
                secrets,
                state.store.as_deref(),
            )
            .await
            {
                Some(recovered_id) => {
                    tracing::info!(
                        warehouse = %warehouse,
                        prefix = %recovered_id,
                        "wire-trino: auto-recovery succeeded; retrying CREATE CATALOG with fresh UUID"
                    );
                    wire_trino_iceberg_catalog(
                        state.store.as_deref(),
                        state.secrets.as_deref(),
                        &lakekeeper_url,
                        &form,
                    )
                    .await
                }
                None => attempt,
            }
        }
        other => other,
    };

    match final_outcome {
        TrinoWiringOutcome::Wired { catalog_name } => axum::response::Redirect::to(&format!(
            "{base_redirect}?wired={}",
            url_encode(&format!(
                "SQL catalog `{catalog_name}` is now registered with Trino. Run `SELECT * FROM {catalog_name}.<schema>.<table>` in the editor."
            ))
        )),
        TrinoWiringOutcome::TrinoUnavailable => err_redirect(
            "Trino isn't installed or discoverable. Install it from /install and try again.",
        ),
        TrinoWiringOutcome::Failed { reason } => err_redirect(&format!(
            "Trino rejected CREATE CATALOG:\n{reason}\n\nMost likely cause: the Trino coordinator is up but Iceberg-REST returned an error (wrong warehouse name, missing project, S3 credentials drift). Inspect /etc/computeza/trino/etc/catalog/ on the host + the Trino log to drill in."
        )),
    }
}

async fn studio_catalog_namespace_create_handler(
    State(state): State<AppState>,
    axum::extract::Path(warehouse): axum::extract::Path<String>,
    axum::extract::Form(form): axum::extract::Form<CreateNamespaceForm>,
) -> axum::response::Redirect {
    let base_redirect = format!("/studio/catalog/{warehouse}");
    let redirect_err = |msg: &str| {
        axum::response::Redirect::to(&format!("{base_redirect}?err={}", url_encode(msg)))
    };
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return redirect_err("Lakekeeper endpoint is not registered.");
    };
    // Multi-level namespaces split on `.` -- "finance.raw" becomes
    // ["finance", "raw"] per Iceberg's namespace model.
    let segments: Vec<String> = form
        .name
        .split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if segments.is_empty() {
        return redirect_err("Schema name is required.");
    }
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let path = format!("/catalog/v1/{}/namespaces", url_encode(&wh_id));
    let body = serde_json::json!({
        "namespace": segments,
        "properties": {},
    });
    if let Err(e) =
        lakekeeper_catalog_mutate(&base_url, reqwest::Method::POST, &path, Some(body)).await
    {
        tracing::warn!(
            warehouse = %warehouse,
            namespace = %form.name,
            error = %e,
            "catalog: namespace create failed"
        );
        return redirect_err(&e);
    }
    axum::response::Redirect::to(&base_redirect)
}

async fn studio_catalog_namespace_delete_handler(
    State(state): State<AppState>,
    axum::extract::Path((warehouse, namespace)): axum::extract::Path<(String, String)>,
) -> axum::response::Redirect {
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return axum::response::Redirect::to(&format!("/studio/catalog/{warehouse}"));
    };
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let ns_encoded = namespace.split('.').collect::<Vec<_>>().join("%1F");
    let path = format!(
        "/catalog/v1/{}/namespaces/{}",
        url_encode(&wh_id),
        ns_encoded
    );
    if let Err(e) =
        lakekeeper_catalog_mutate(&base_url, reqwest::Method::DELETE, &path, None).await
    {
        tracing::warn!(
            warehouse = %warehouse,
            namespace = %namespace,
            error = %e,
            "catalog: namespace delete failed"
        );
    }
    axum::response::Redirect::to(&format!("/studio/catalog/{warehouse}"))
}

#[derive(serde::Deserialize)]
struct CreateTableForm {
    name: String,
    /// Multi-line text where each line is `column_name:iceberg_type`.
    /// Iceberg primitive types: long, int, string, boolean, double,
    /// float, date, timestamp, timestamptz, binary.
    columns: String,
}

async fn studio_catalog_table_create_handler(
    State(state): State<AppState>,
    axum::extract::Path((warehouse, namespace)): axum::extract::Path<(String, String)>,
    axum::extract::Form(form): axum::extract::Form<CreateTableForm>,
) -> axum::response::Redirect {
    let base_redirect = format!("/studio/catalog/{warehouse}/{namespace}");
    let redirect_err = |msg: &str| {
        axum::response::Redirect::to(&format!("{base_redirect}?err={}", url_encode(msg)))
    };
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return redirect_err("Lakekeeper endpoint is not registered.");
    };
    let table_name = form.name.trim();
    if table_name.is_empty() {
        return redirect_err("Table name is required.");
    }
    // Parse "name:type" lines into Iceberg schema fields. Ignore
    // blank lines + lines starting with #.
    let fields: Vec<serde_json::Value> = form
        .columns
        .lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                return None;
            }
            let (name, ty) = t.split_once(':')?;
            let n = name.trim();
            let y = ty.trim();
            if n.is_empty() || y.is_empty() {
                return None;
            }
            Some((n.to_string(), y.to_string()))
        })
        .enumerate()
        .map(|(idx, (name, ty))| {
            serde_json::json!({
                "id": (idx as i64) + 1,
                "name": name,
                "type": ty,
                "required": false,
            })
        })
        .collect();
    if fields.is_empty() {
        return redirect_err(
            "No valid columns parsed. Each non-blank, non-comment line must look like `name:type`.",
        );
    }
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let ns_encoded = namespace.split('.').collect::<Vec<_>>().join("%1F");
    let path = format!(
        "/catalog/v1/{}/namespaces/{}/tables",
        url_encode(&wh_id),
        ns_encoded
    );
    // Iceberg-REST CreateTableRequest. The schema must carry a
    // `schema-id` (Lakekeeper rejects without it) and `partition-spec`
    // must be an OBJECT (UnboundPartitionSpec) -- the legacy code sent
    // an array which Lakekeeper rejects as invalid JSON shape. We
    // omit partition-spec entirely for the unpartitioned default; the
    // server fills in spec-id=0 / empty fields.
    let body = serde_json::json!({
        "name": table_name,
        "schema": {
            "type": "struct",
            "schema-id": 0,
            "fields": fields,
        },
        "properties": {},
    });
    if let Err(e) =
        lakekeeper_catalog_mutate(&base_url, reqwest::Method::POST, &path, Some(body)).await
    {
        tracing::warn!(
            warehouse = %warehouse,
            namespace = %namespace,
            table = %table_name,
            error = %e,
            "catalog: table create failed"
        );
        return redirect_err(&e);
    }
    axum::response::Redirect::to(&base_redirect)
}

async fn studio_catalog_table_delete_handler(
    State(state): State<AppState>,
    axum::extract::Path((warehouse, namespace, table)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
) -> axum::response::Redirect {
    let redirect_to = format!("/studio/catalog/{warehouse}/{namespace}");
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return axum::response::Redirect::to(&redirect_to);
    };
    let wh_id = resolve_warehouse_id_or_pass(&warehouse, Some(&base_url), state.secrets.as_deref()).await;
    let ns_encoded = namespace.split('.').collect::<Vec<_>>().join("%1F");
    let path = format!(
        "/catalog/v1/{}/namespaces/{}/tables/{}",
        url_encode(&wh_id),
        ns_encoded,
        url_encode(&table)
    );
    if let Err(e) =
        lakekeeper_catalog_mutate(&base_url, reqwest::Method::DELETE, &path, None).await
    {
        tracing::warn!(
            warehouse = %warehouse,
            namespace = %namespace,
            table = %table,
            error = %e,
            "catalog: table delete failed"
        );
    }
    axum::response::Redirect::to(&redirect_to)
}

/// Breadcrumb-only error page for the drill-down routes. Used
/// when the prerequisites aren't met (Lakekeeper missing) so the
/// operator sees consistent navigation regardless of state.
fn render_studio_drilldown_error(
    localizer: &Localizer,
    crumbs: &[(&str, &str)],
    message: &str,
    sidebar: &str,
) -> String {
    let breadcrumbs = render_drilldown_breadcrumbs(crumbs);
    let main_html = format!(
        r#"{breadcrumbs}
<div class="cz-studio-error">{}</div>"#,
        html_escape(message)
    );
    let body = format!(
        r#"<div class="cz-studio-shell">
<aside class="cz-studio-sidebar">{sidebar}</aside>
<main class="cz-studio-main">{main_html}</main>
</div>"#
    );
    render_shell(localizer, "Catalog", NavLink::Studio, &body)
}

/// Render the breadcrumb bar at the top of a drill-down main pane.
/// Uses the new `.cz-studio-crumbs` styling.
fn render_drilldown_breadcrumbs(crumbs: &[(&str, &str)]) -> String {
    let mut out = String::from(
        r#"<div class="cz-studio-crumbs"><a href="/studio">Studio</a><span class="cz-studio-crumbs-sep">/</span><a href="/studio">Catalog</a>"#,
    );
    let last = crumbs.len().saturating_sub(1);
    for (i, (label, href)) in crumbs.iter().enumerate() {
        out.push_str(r#"<span class="cz-studio-crumbs-sep">/</span>"#);
        if i == last {
            // Last crumb is the current location -- styled distinctly,
            // not a link.
            out.push_str(&format!(
                r#"<span class="cz-studio-crumbs-current">{}</span>"#,
                html_escape(label)
            ));
        } else {
            out.push_str(&format!(
                r#"<a href="{}"><code style="font-size: 0.78rem;">{}</code></a>"#,
                html_escape(href),
                html_escape(label)
            ));
        }
    }
    out.push_str("</div>");
    out
}

/// Build the full sidebar HTML used on every Studio page: the
/// expandable catalog tree (warehouses → namespaces → tables, with
/// the focused branch open) plus the Recent Queries section. Each
/// page passes the operator's current focus so the right branch
/// renders expanded and the right row gets `cz-tree-active`.
async fn build_studio_full_sidebar(state: &AppState, focus: SidebarFocus<'_>) -> String {
    let tree = build_studio_sidebar_tree(state, focus).await;
    let tree_html = render_studio_tree_sidebar(&tree, focus);
    let history = load_studio_history(state.store.as_deref()).await;
    let recent_html = render_sidebar_recent_queries(&history, "Reload query");
    // Recent is collapsed by default. <details> remembers per-page
    // state in the URL via :target if we want; for now it's a fresh
    // collapsed view on every render so operators always start with
    // a clean rail. Click the eyebrow to expand.
    format!(
        r#"{tree_html}
<section class="cz-studio-sidebar-section">
<details class="cz-studio-recent-details">
<summary class="cz-studio-sidebar-eyebrow cz-studio-recent-summary"><span><span class="cz-studio-recent-chevron"></span>Recent ({n})</span></summary>
<div class="cz-studio-recent-body">{recent_html}</div>
</details>
</section>"#,
        n = history.entries.len(),
    )
}

/// In-memory shape of the full sidebar tree at render time. Built by
/// `build_studio_sidebar_tree` from live Lakekeeper state + the
/// current focus path; rendered by `render_studio_tree_sidebar`.
#[derive(Debug, Default)]
struct StudioSidebarTree {
    warehouses: Vec<SidebarWarehouseNode>,
}

#[derive(Debug)]
struct SidebarWarehouseNode {
    name: String,
    /// `None` when this warehouse isn't on the focus path -- the tree
    /// renders it as a collapsed `<details>` and clicking expands it
    /// via navigation. `Some(Ok(...))` when this is the focused
    /// warehouse and namespaces were fetched. `Some(Err(...))` when
    /// fetch failed -- inline error in the tree.
    namespaces: Option<Result<Vec<SidebarNamespaceNode>, String>>,
}

#[derive(Debug)]
struct SidebarNamespaceNode {
    qualified: String,
    tables: Option<Result<Vec<String>, String>>,
}

/// Where the user currently is. Drives which subtrees render expanded
/// and which row gets `cz-tree-active`.
#[derive(Debug, Default, Clone, Copy)]
struct SidebarFocus<'a> {
    warehouse: Option<&'a str>,
    namespace: Option<&'a str>,
    table: Option<&'a str>,
}

/// Fetch the warehouse list (always) + drill one level deeper for the
/// currently focused warehouse + namespace. Each tier is best-effort:
/// if a network call fails we attach the error to the subtree so the
/// sidebar still renders.
async fn build_studio_sidebar_tree(
    state: &AppState,
    focus: SidebarFocus<'_>,
) -> StudioSidebarTree {
    let Some(base_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return StudioSidebarTree::default();
    };
    let lk_state =
        probe_lakekeeper_with_vault_fallback(&base_url, state.secrets.as_deref()).await;
    let names: Vec<String> = match lk_state {
        LakekeeperState::HasWarehouses(n) => n,
        _ => Vec::new(),
    };
    let mut warehouses: Vec<SidebarWarehouseNode> = Vec::with_capacity(names.len());
    for name in names {
        let namespaces = if focus.warehouse == Some(name.as_str()) {
            Some(load_namespaces_for_sidebar(state, &base_url, &name, focus).await)
        } else {
            None
        };
        warehouses.push(SidebarWarehouseNode { name, namespaces });
    }
    StudioSidebarTree { warehouses }
}

/// Fetch a warehouse's namespace list and, if focus has a current
/// namespace, fetch its tables too. Returns Ok with the namespace
/// nodes (each optionally populated with table children) or Err with
/// the Lakekeeper error message.
async fn load_namespaces_for_sidebar(
    state: &AppState,
    base_url: &str,
    warehouse: &str,
    focus: SidebarFocus<'_>,
) -> Result<Vec<SidebarNamespaceNode>, String> {
    let wh_id =
        resolve_warehouse_id_or_pass(warehouse, Some(base_url), state.secrets.as_deref()).await;
    let path = format!("/catalog/v1/{}/namespaces", url_encode(&wh_id));
    let v = get_lakekeeper_catalog_json(base_url, &path).await?;
    let qualified_names: Vec<String> = v
        .get("namespaces")
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ns| {
                    let segs: Vec<String> = ns
                        .as_array()?
                        .iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect();
                    if segs.is_empty() {
                        return None;
                    }
                    Some(segs.join("."))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut nodes: Vec<SidebarNamespaceNode> = Vec::with_capacity(qualified_names.len());
    for qualified in qualified_names {
        let tables = if focus.namespace == Some(qualified.as_str()) {
            let ns_encoded = qualified.split('.').collect::<Vec<_>>().join("%1F");
            let tpath = format!(
                "/catalog/v1/{}/namespaces/{}/tables",
                url_encode(&wh_id),
                ns_encoded
            );
            Some(
                get_lakekeeper_catalog_json(base_url, &tpath)
                    .await
                    .map(|tv| {
                        tv.get("identifiers")
                            .and_then(|n| n.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|t| {
                                        t.get("name")
                                            .and_then(|n| n.as_str())
                                            .map(str::to_string)
                                    })
                                    .collect::<Vec<String>>()
                            })
                            .unwrap_or_default()
                    }),
            )
        } else {
            None
        };
        nodes.push(SidebarNamespaceNode { qualified, tables });
    }
    Ok(nodes)
}

/// Render the persistent expandable tree sidebar. Uses `<details>`
/// elements so expand/collapse works without JS. The focused branch
/// renders with `open`; other warehouses render collapsed with their
/// names only (clicking navigates, which on the next page-load shows
/// them expanded).
fn render_studio_tree_sidebar(tree: &StudioSidebarTree, focus: SidebarFocus<'_>) -> String {
    if tree.warehouses.is_empty() {
        return format!(
            r#"<section class="cz-studio-sidebar-section">
<div class="cz-studio-sidebar-eyebrow"><span>Catalog</span><a href="/studio/bootstrap" class="cz-studio-sidebar-action" title="Bootstrap a warehouse">+</a></div>
<p class="cz-studio-sidebar-note">No warehouses yet. <a href="/studio/bootstrap" style="color: var(--lavender);">Bootstrap one</a> to start.</p>
</section>"#,
        );
    }
    let items: String = tree
        .warehouses
        .iter()
        .map(|wh| render_sidebar_warehouse(wh, focus))
        .collect();
    format!(
        r#"<section class="cz-studio-sidebar-section">
<div class="cz-studio-sidebar-eyebrow"><span>Catalog</span><a href="/studio/bootstrap" class="cz-studio-sidebar-action" title="Bootstrap a warehouse">+</a></div>
<ul class="cz-tree">{items}</ul>
</section>"#,
    )
}

fn render_sidebar_warehouse(node: &SidebarWarehouseNode, focus: SidebarFocus<'_>) -> String {
    let is_current = focus.warehouse == Some(node.name.as_str());
    let href = format!("/studio/catalog/{}", url_encode(&node.name));
    let active = if is_current && focus.namespace.is_none() {
        " cz-tree-active"
    } else {
        ""
    };
    let open_attr = if is_current { " open" } else { "" };
    let children_html = match &node.namespaces {
        Some(Ok(nss)) if nss.is_empty() => format!(
            r#"<li class="cz-tree-empty">No namespaces yet.</li>"#
        ),
        Some(Ok(nss)) => nss
            .iter()
            .map(|ns| render_sidebar_namespace(&node.name, ns, focus))
            .collect::<String>(),
        Some(Err(e)) => format!(
            r#"<li class="cz-tree-error" title="{}">⚠ load failed</li>"#,
            html_escape(e)
        ),
        None => String::new(),
    };
    let children_block = if is_current {
        format!(r#"<ul class="cz-tree-children">{children_html}</ul>"#)
    } else {
        String::new()
    };
    format!(
        r#"<li class="cz-tree-item"><details class="cz-tree-details"{open_attr}>
<summary class="cz-tree-row{active}"><span class="cz-tree-toggle"></span><span class="cz-tree-icon">⬢</span><a class="cz-tree-link" href="{href}">{label}</a></summary>
{children_block}
</details></li>"#,
        href = html_escape(&href),
        label = html_escape(&node.name),
    )
}

fn render_sidebar_namespace(
    warehouse: &str,
    node: &SidebarNamespaceNode,
    focus: SidebarFocus<'_>,
) -> String {
    let is_current = focus.namespace == Some(node.qualified.as_str());
    let href = format!(
        "/studio/catalog/{}/{}",
        url_encode(warehouse),
        url_encode(&node.qualified)
    );
    let active = if is_current && focus.table.is_none() {
        " cz-tree-active"
    } else {
        ""
    };
    let open_attr = if is_current { " open" } else { "" };
    let children_html = match &node.tables {
        Some(Ok(tbls)) if tbls.is_empty() => {
            r#"<li class="cz-tree-empty">No tables yet.</li>"#.to_string()
        }
        Some(Ok(tbls)) => tbls
            .iter()
            .map(|t| render_sidebar_table(warehouse, &node.qualified, t, focus))
            .collect(),
        Some(Err(e)) => format!(
            r#"<li class="cz-tree-error" title="{}">⚠ load failed</li>"#,
            html_escape(e)
        ),
        None => String::new(),
    };
    let children_block = if is_current {
        format!(r#"<ul class="cz-tree-children">{children_html}</ul>"#)
    } else {
        String::new()
    };
    format!(
        r#"<li class="cz-tree-item"><details class="cz-tree-details"{open_attr}>
<summary class="cz-tree-row{active}"><span class="cz-tree-toggle"></span><span class="cz-tree-icon">◇</span><a class="cz-tree-link" href="{href}">{label}</a></summary>
{children_block}
</details></li>"#,
        href = html_escape(&href),
        label = html_escape(&node.qualified),
    )
}

fn render_sidebar_table(
    warehouse: &str,
    namespace: &str,
    table: &str,
    focus: SidebarFocus<'_>,
) -> String {
    let is_current = focus.table == Some(table);
    let href = format!(
        "/studio/catalog/{}/{}/{}",
        url_encode(warehouse),
        url_encode(namespace),
        url_encode(table)
    );
    let active = if is_current { " cz-tree-active" } else { "" };
    format!(
        r#"<li class="cz-tree-item"><a class="cz-tree-row cz-tree-leaf{active}" href="{href}"><span class="cz-tree-toggle"></span><span class="cz-tree-icon">▤</span><span class="cz-tree-label">{label}</span></a></li>"#,
        href = html_escape(&href),
        label = html_escape(table),
    )
}

fn render_studio_namespace_list(
    localizer: &Localizer,
    warehouse: &str,
    result: Result<serde_json::Value, String>,
    recovery_banner: Option<&str>,
    err_banner: Option<&str>,
    wired_banner: Option<&str>,
    sidebar: &str,
) -> String {
    let crumbs = [(warehouse, &*format!("/studio/catalog/{warehouse}"))];
    let breadcrumbs = render_drilldown_breadcrumbs(&crumbs);
    let csrf = auth::csrf_input();
    // Banner precedence: wiring-success (green) > error (red) >
    // auto-recovery (amber). Recovery is the noisiest and is what was
    // labelled "State-mismatch recovery"; raw POST errors get their
    // own label so the operator isn't misled into thinking a wiring
    // error came from auto-recovery.
    let banner_html = if let Some(msg) = wired_banner {
        format!(
            r#"<div class="cz-studio-banner" style="background: rgba(168, 232, 196, 0.06); border-color: rgba(168, 232, 196, 0.3); color: var(--ok);"><strong>SQL access wired:</strong> {}</div>"#,
            html_escape(msg)
        )
    } else if let Some(msg) = err_banner {
        format!(
            r#"<div class="cz-studio-error" style="margin: 0 0 1.25rem;">{}</div>"#,
            html_escape(msg)
        )
    } else if let Some(msg) = recovery_banner {
        format!(
            r#"<div class="cz-studio-banner"><strong>State-mismatch recovery:</strong> {}</div>"#,
            html_escape(msg)
        )
    } else {
        String::new()
    };

    let (list_html, namespace_count) = match result {
        Ok(v) => {
            let namespaces = v
                .get("namespaces")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            if namespaces.is_empty() {
                (
                    r##"<div class="cz-studio-empty">
<span class="cz-studio-empty-icon">⊕</span>
<div class="cz-studio-empty-title">No schemas yet</div>
<p class="cz-studio-empty-text">Schemas group related tables. Use dotted names (<code>finance.raw</code>) for nested schemas.</p>
<a href="#cz-modal-new-schema" class="cz-btn-primary">+ Create your first schema</a>
</div>"##.to_string(),
                    0,
                )
            } else {
                let items: String = namespaces
                    .iter()
                    .filter_map(|ns| {
                        let segments: Vec<String> = ns
                            .as_array()?
                            .iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect();
                        if segments.is_empty() {
                            return None;
                        }
                        let qualified = segments.join(".");
                        Some(format!(
                            r#"<li class="cz-studio-list-item">
<a href="/studio/catalog/{wh}/{ns}"><span style="opacity:0.6;">▸</span><code>{ns_display}</code></a>
<form method="post" action="/studio/catalog/{wh}/{ns}/delete" style="margin: 0;" onsubmit="return confirm('Delete schema {ns_display}? This is destructive and fails if the schema has tables.');">{csrf}
<button type="submit" class="cz-btn-danger">Delete</button>
</form>
</li>"#,
                            wh = url_encode(warehouse),
                            ns = url_encode(&qualified),
                            ns_display = html_escape(&qualified),
                            csrf = csrf,
                        ))
                    })
                    .collect();
                let count = namespaces.len();
                (
                    format!(r#"<ul class="cz-studio-list">{items}</ul>"#),
                    count,
                )
            }
        }
        Err(e) => (format!(r#"<pre class="cz-studio-error">{}</pre>"#, html_escape(&e)), 0),
    };

    let create_modal = format!(
        r##"<div id="cz-modal-new-schema" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-modal-new-schema-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-modal-new-schema-title" class="cz-modal-title">New schema</h2>
<p class="cz-modal-subtitle">Schemas group related tables — like databases. Dotted names like <code>finance.raw</code> become nested schemas.</p>
<form method="post" action="/studio/catalog/{wh}/namespaces/create">{csrf}
<div class="cz-modal-field">
<label for="ns-create-name">Schema name</label>
<input id="ns-create-name" name="name" class="cz-input" type="text" placeholder="finance" required autofocus />
<p class="cz-modal-field-hint">Iceberg stores nested schemas as arrays (<code>["finance", "raw"]</code>); the dotted form is just shorthand.</p>
</div>
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-primary">Create schema</button>
</div>
</form>
</div>
</div>"##,
        wh = url_encode(warehouse),
        csrf = csrf,
    );

    let count_label = match namespace_count {
        0 => "".to_string(),
        n => format!(r#" <span class="cz-studio-results-count">({n})</span>"#),
    };

    let wire_form = format!(
        r#"<form method="post" action="/studio/catalog/{wh_enc}/wire-trino" style="margin: 0;" title="Re-fire CREATE CATALOG against Trino so the SQL editor can read tables in this warehouse. Uses credentials cached in vault from the original bootstrap.">{csrf}
<button type="submit" class="cz-btn-ghost">Connect to SQL</button>
</form>"#,
        wh_enc = url_encode(warehouse),
        csrf = csrf,
    );
    let main_html = format!(
        r##"{breadcrumbs}
{banner_html}
<div class="cz-studio-actions" style="margin-bottom: 0.5rem;">
<div>
<h1 class="cz-studio-pane-title" style="margin: 0;">Schemas in <code>{wh}</code>{count_label}</h1>
<p class="cz-studio-pane-subtitle" style="margin: 0.25rem 0 0;">Schemas group tables. Click a schema to drill into its tables.</p>
</div>
<span class="cz-studio-actions-spacer"></span>
{wire_form}
<a href="#cz-modal-new-schema" class="cz-btn-primary">+ New schema</a>
</div>
{list_html}
{create_modal}"##,
        wh = html_escape(warehouse),
    );
    let body = format!(
        r#"<div class="cz-studio-shell">
<aside class="cz-studio-sidebar">{sidebar}</aside>
<main class="cz-studio-main">{main_html}</main>
</div>"#
    );
    render_shell(
        localizer,
        &format!("Catalog: {warehouse}"),
        NavLink::Studio,
        &body,
    )
}

fn render_studio_table_list(
    localizer: &Localizer,
    warehouse: &str,
    namespace: &str,
    result: Result<serde_json::Value, String>,
    error_banner: Option<&str>,
    sidebar: &str,
) -> String {
    let ns_href = format!("/studio/catalog/{warehouse}/{namespace}");
    let crumbs = [
        (warehouse, &*format!("/studio/catalog/{warehouse}")),
        (namespace, &*ns_href),
    ];
    let breadcrumbs = render_drilldown_breadcrumbs(&crumbs);
    let csrf = auth::csrf_input();
    let banner_html = error_banner
        .map(|msg| {
            format!(
                r#"<div class="cz-studio-banner"><strong>Create failed:</strong> <pre style="white-space: pre-wrap; margin: 0.4rem 0 0; font-family: 'Geist Mono', ui-monospace, monospace; font-size: 0.78rem;">{}</pre></div>"#,
                html_escape(msg)
            )
        })
        .unwrap_or_default();

    let (list_html, table_count) = match result {
        Ok(v) => {
            let tables = v
                .get("identifiers")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            if tables.is_empty() {
                (
                    r##"<div class="cz-studio-empty">
<span class="cz-studio-empty-icon">▤</span>
<div class="cz-studio-empty-title">No tables yet</div>
<p class="cz-studio-empty-text">Define a table with a name and a list of typed columns (one per line). Iceberg primitives like <code>long</code>, <code>string</code>, <code>timestamp</code> are accepted.</p>
<a href="#cz-modal-new-table" class="cz-btn-primary">+ Create your first table</a>
</div>"##.to_string(),
                    0,
                )
            } else {
                let items: String = tables
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(|name| {
                        format!(
                            r#"<li class="cz-studio-list-item">
<a href="/studio/catalog/{wh}/{ns}/{tbl}"><span style="opacity:0.6;">▤</span><code>{tbl_display}</code></a>
<form method="post" action="/studio/catalog/{wh}/{ns}/{tbl}/delete" style="margin: 0;" onsubmit="return confirm('Delete table {tbl_display}? This is destructive.');">{csrf}
<button type="submit" class="cz-btn-danger">Delete</button>
</form>
</li>"#,
                            wh = url_encode(warehouse),
                            ns = url_encode(namespace),
                            tbl = url_encode(name),
                            tbl_display = html_escape(name),
                            csrf = csrf,
                        )
                    })
                    .collect();
                let count = tables.len();
                (
                    format!(r#"<ul class="cz-studio-list">{items}</ul>"#),
                    count,
                )
            }
        }
        Err(e) => (format!(r#"<pre class="cz-studio-error">{}</pre>"#, html_escape(&e)), 0),
    };

    let create_modal = format!(
        r##"<div id="cz-modal-new-table" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-modal-new-table-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal" style="max-width: 36rem;">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-modal-new-table-title" class="cz-modal-title">New table in <code>{ns_display}</code></h2>
<p class="cz-modal-subtitle">Define an Iceberg table — a name and a list of typed columns, one per line.</p>
<form method="post" action="/studio/catalog/{wh}/{ns}/tables/create">{csrf}
<div class="cz-modal-field">
<label for="tbl-create-name">Table name</label>
<input id="tbl-create-name" name="name" class="cz-input" type="text" placeholder="customers" required autofocus />
</div>
<div class="cz-modal-field">
<label for="tbl-create-cols">Columns (<code>name:type</code>, one per line)</label>
<textarea id="tbl-create-cols" name="columns" rows="8" class="cz-input" placeholder="id:long&#10;name:string&#10;email:string&#10;created_at:timestamp" required style="font-size: 0.82rem; line-height: 1.5;"></textarea>
<p class="cz-modal-field-hint">Iceberg primitives: <code>long</code>, <code>int</code>, <code>string</code>, <code>boolean</code>, <code>double</code>, <code>float</code>, <code>date</code>, <code>timestamp</code>, <code>timestamptz</code>, <code>binary</code>. Blank lines + <code>#</code> comments are ignored.</p>
</div>
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-primary">Create table</button>
</div>
</form>
</div>
</div>"##,
        wh = url_encode(warehouse),
        ns = url_encode(namespace),
        ns_display = html_escape(namespace),
        csrf = csrf,
    );

    let count_label = match table_count {
        0 => "".to_string(),
        n => format!(r#" <span class="cz-studio-results-count">({n})</span>"#),
    };

    let main_html = format!(
        r##"{breadcrumbs}
{banner_html}
<div class="cz-studio-actions" style="margin-bottom: 0.5rem;">
<div>
<h1 class="cz-studio-pane-title" style="margin: 0;">Tables in <code>{wh}</code>.<code>{ns}</code>{count_label}</h1>
<p class="cz-studio-pane-subtitle" style="margin: 0.25rem 0 0;">Iceberg tables under this schema. Click a table to inspect its columns.</p>
</div>
<span class="cz-studio-actions-spacer"></span>
<a href="#cz-modal-new-table" class="cz-btn-primary">+ New table</a>
</div>
{list_html}
{create_modal}"##,
        wh = html_escape(warehouse),
        ns = html_escape(namespace),
    );
    let body = format!(
        r#"<div class="cz-studio-shell">
<aside class="cz-studio-sidebar">{sidebar}</aside>
<main class="cz-studio-main">{main_html}</main>
</div>"#
    );
    render_shell(
        localizer,
        &format!("Catalog: {warehouse}.{namespace}"),
        NavLink::Studio,
        &body,
    )
}

fn render_studio_table_detail(
    localizer: &Localizer,
    warehouse: &str,
    namespace: &str,
    table: &str,
    result: Result<serde_json::Value, String>,
    sidebar: &str,
) -> String {
    let ns_href = format!("/studio/catalog/{warehouse}/{namespace}");
    let tbl_href = format!("/studio/catalog/{warehouse}/{namespace}/{table}");
    let crumbs = [
        (warehouse, &*format!("/studio/catalog/{warehouse}")),
        (namespace, &*ns_href),
        (table, &*tbl_href),
    ];
    let breadcrumbs = render_drilldown_breadcrumbs(&crumbs);
    let body_inner = match result {
        Ok(v) => {
            let location = v
                .get("metadata")
                .and_then(|m| m.get("location"))
                .or_else(|| v.get("location"))
                .and_then(|x| x.as_str())
                .unwrap_or("(unknown)");
            let current_snapshot = v
                .get("metadata")
                .and_then(|m| m.get("current-snapshot-id"))
                .or_else(|| v.get("current-snapshot-id"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(none)".to_string());
            let format_version = v
                .get("metadata")
                .and_then(|m| m.get("format-version"))
                .map(|x| x.to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            let schema_html = v
                .get("metadata")
                .and_then(|m| m.get("schemas"))
                .and_then(|s| s.as_array())
                .and_then(|arr| arr.first())
                .and_then(|s| s.get("fields"))
                .and_then(|f| f.as_array())
                .map(|fields| {
                    let rows: String = fields
                        .iter()
                        .map(|f| {
                            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let ty = f
                                .get("type")
                                .map(|t| t.to_string().replace('"', ""))
                                .unwrap_or_else(|| "?".into());
                            let nullable = f
                                .get("required")
                                .and_then(|r| r.as_bool())
                                .map(|r| if r { "NOT NULL" } else { "NULL OK" })
                                .unwrap_or("?");
                            format!(
                                "<tr><td>{}</td><td>{}</td><td class=\"cz-muted\">{}</td></tr>",
                                html_escape(name),
                                html_escape(&ty),
                                nullable
                            )
                        })
                        .collect();
                    format!(
                        r#"<table class="cz-studio-schema">
<thead><tr><th>Column</th><th>Type</th><th>Nullable</th></tr></thead>
<tbody>{rows}</tbody>
</table>"#
                    )
                })
                .unwrap_or_else(|| {
                    r#"<div class="cz-studio-results-empty">No schema found in table metadata.</div>"#
                        .to_string()
                });
            let raw_json = serde_json::to_string_pretty(&v)
                .unwrap_or_else(|_| "(could not pretty-print)".to_string());
            let catalog_ident = sanitize_sql_identifier(warehouse);
            let prefill_sql = format!(
                "SELECT * FROM {catalog_ident}.{namespace}.{table} LIMIT 100"
            );
            format!(
                r#"<dl class="cz-dl">
<dt>Location</dt><dd><code>{loc}</code> <span class="cz-muted" style="font-size: 0.72rem; margin-left: 0.4rem;">(local Garage — on-disk under <code>/var/lib/computeza/garage</code>; the <code>s3://</code> URI is the protocol, not a cloud bucket)</span></dd>
<dt>Current snapshot</dt><dd><code>{snap}</code></dd>
<dt>Format version</dt><dd><code>{fmt}</code></dd>
</dl>
<h2 class="cz-studio-section-heading">Schema</h2>
{schema_html}
<div class="cz-studio-actions" style="margin-top: 1.25rem;">
<a href="/studio?sql={prefill_enc}" class="cz-btn-primary">Pre-fill SELECT * in editor</a>
<span class="cz-studio-actions-spacer"></span>
<span class="cz-studio-editor-hint" style="margin: 0;">Reads <code>{catalog_ident}.{ns_display}.{tbl_display}</code> via the Trino catalog the bootstrap step registered. If the editor can't find this catalog, re-run <a href="/studio/bootstrap">bootstrap</a> with Trino installed.</span>
</div>
<details style="margin-top: 1.75rem;">
<summary class="cz-studio-raw-summary">Raw Iceberg metadata (JSON)</summary>
<pre class="cz-studio-raw">{raw}</pre>
</details>"#,
                loc = html_escape(location),
                snap = html_escape(&current_snapshot),
                fmt = html_escape(&format_version),
                schema_html = schema_html,
                prefill_enc = url_encode(&prefill_sql),
                catalog_ident = html_escape(&catalog_ident),
                ns_display = html_escape(namespace),
                tbl_display = html_escape(table),
                raw = html_escape(&raw_json),
            )
        }
        Err(e) => format!(r#"<pre class="cz-studio-error">{}</pre>"#, html_escape(&e)),
    };
    let main_html = format!(
        r#"{breadcrumbs}
<h1 class="cz-studio-pane-title"><code>{wh}</code>.<code>{ns}</code>.<code>{tbl}</code></h1>
<p class="cz-studio-pane-subtitle">Iceberg table metadata — schema, location, current snapshot. Use the editor to query rows.</p>
{body_inner}"#,
        wh = html_escape(warehouse),
        ns = html_escape(namespace),
        tbl = html_escape(table),
    );
    let body = format!(
        r#"<div class="cz-studio-shell">
<aside class="cz-studio-sidebar">{sidebar}</aside>
<main class="cz-studio-main">{main_html}</main>
</div>"#
    );
    render_shell(
        localizer,
        &format!("Catalog: {warehouse}.{namespace}.{table}"),
        NavLink::Studio,
        &body,
    )
}

/// Query parameters accepted by the studio page. `sql` lets the
/// catalog browser's "Pre-fill SELECT *" link populate the editor
/// without JavaScript -- the link hits `/studio?sql=...` and the
/// renderer drops the value into the textarea.
#[derive(serde::Deserialize, Default)]
struct StudioQuery {
    sql: Option<String>,
    /// Comma-separated file IDs currently open as tabs. Persists tab
    /// state across navigation/refresh without JS storage. Empty/None
    /// = no files open; editor uses its sql_prefill default.
    open: Option<String>,
    /// File ID of the currently focused tab. Must be present in
    /// `open`; if not, the renderer falls back to the first id in
    /// `open` (or no file at all).
    active: Option<String>,
    /// Flash banner after a file action (save/import/etc).
    flash: Option<String>,
}

/// In-memory snapshot of the workspace-files state for one render
/// pass: the full file list (for the tree), the subset open as tabs
/// (in operator-chosen order), and which one is currently focused.
/// The editor's textarea is pre-filled with the active file's content
/// when no explicit `?sql=` override is present.
struct StudioFilesView {
    all: Vec<computeza_state::StudioFile>,
    open: Vec<computeza_state::StudioFile>,
    active_id: Option<String>,
    flash: Option<String>,
}

impl StudioFilesView {
    fn active_file(&self) -> Option<&computeza_state::StudioFile> {
        let id = self.active_id.as_deref()?;
        self.open.iter().find(|f| f.id == id)
    }
    fn open_csv(&self) -> String {
        self.open
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Build the StudioFilesView from the current request query. Handles
/// missing store gracefully (returns an empty view, so the editor
/// works without persisted files).
async fn build_studio_files_view(
    state: &AppState,
    q: &StudioQuery,
) -> StudioFilesView {
    let Some(store) = state.store.as_deref() else {
        return StudioFilesView {
            all: Vec::new(),
            open: Vec::new(),
            active_id: None,
            flash: q.flash.clone(),
        };
    };
    let all = store.studio_files_list().await.unwrap_or_default();
    // Parse the `open` csv and resolve each id against `all`. Ids
    // that aren't found (deleted file with stale URL) are silently
    // dropped so the tab strip stays consistent with reality.
    let requested_ids: Vec<String> = q
        .open
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let open: Vec<computeza_state::StudioFile> = requested_ids
        .into_iter()
        .filter_map(|id| all.iter().find(|f| f.id == id).cloned())
        .collect();
    // Active id must be present in `open`. If not, fall back to the
    // last id in `open` (most recently added tab) so the editor isn't
    // showing a tab strip with nothing highlighted.
    let active_id = match q.active.as_deref() {
        Some(id) if open.iter().any(|f| f.id == id) => Some(id.to_string()),
        _ => open.last().map(|f| f.id.clone()),
    };
    StudioFilesView {
        all,
        open,
        active_id,
        flash: q.flash.clone(),
    }
}

async fn studio_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<StudioQuery>,
) -> Html<String> {
    let l = Localizer::english();
    let lakekeeper = discover_lakekeeper_endpoint(state.store.as_deref()).await;
    let trino = discover_trino_endpoint(state.store.as_deref()).await;
    let lk_state = match &lakekeeper {
        Some(url) => probe_lakekeeper_with_vault_fallback(url, state.secrets.as_deref()).await,
        None => LakekeeperState::Unreachable,
    };
    let history = load_studio_history(state.store.as_deref()).await;
    let files = build_studio_files_view(&state, &q).await;
    // sql_prefill priority: explicit ?sql= override beats the active
    // file's content. Lets the catalog "Pre-fill SELECT *" deep link
    // overwrite the editor body even when a file tab is open.
    let sql_prefill = q.sql.clone().unwrap_or_else(|| {
        files
            .active_file()
            .map(|f| f.content.clone())
            .unwrap_or_default()
    });
    let sidebar = build_studio_full_sidebar(&state, SidebarFocus::default()).await;
    Html(render_studio_page(
        &l,
        lakekeeper.is_some(),
        trino.is_some(),
        &lk_state,
        &history,
        &sql_prefill,
        None,
        &sidebar,
        &files,
    ))
}

/// Form body for `POST /studio/sql/execute`. Plain
/// `application/x-www-form-urlencoded` so the bare HTML form works
/// without JS.
#[derive(serde::Deserialize)]
struct StudioSqlForm {
    sql: String,
    /// Tab state forwarded through the run-query POST so the
    /// editor returns to the same set of open tabs (with the same
    /// active id) after the redirect-less re-render.
    #[serde(default)]
    open: String,
    #[serde(default)]
    active: String,
}

// Note: a previous iteration of this file shipped a
// studio_create_namespace_handler that POSTed to
// /catalog/v1/namespaces. That endpoint is gated behind a
// Lakekeeper warehouse bootstrap that v0.0.x doesn't automate, so
// it 404'd every time. The handler is gone -- catalog mutations
// land in phase 1.5 after the bootstrap wizard (project +
// warehouse + Garage storage profile) ships. See AGENTS.md
// "Deferred work: Lakekeeper bootstrap".

// ============================================================
// Studio workspace files
// ============================================================
//
// CRUD over computeza_state's studio_files table, plus single-file
// import/export and (TODO) .cptz workspace-archive build/parse.
//
// Every mutation redirects back to the editor with:
//   ?open=<csv of ids>&active=<id>&flash=<msg>
// so the operator stays in the editor flow and the tab strip
// reflects the new state on next render.

#[derive(serde::Deserialize)]
struct FileNewForm {
    /// Comma-separated currently-open tabs. Preserved across the
    /// create POST so the new file lands as an additional tab next
    /// to the existing ones.
    #[serde(default)]
    open: String,
    /// Initial path. Defaults to "/untitled-<N>.sql" if blank.
    #[serde(default)]
    path: String,
    /// Initial content (typically the current editor body so the
    /// "New from current" UX feels natural).
    #[serde(default)]
    content: String,
}

async fn studio_file_create_handler(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<FileNewForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(None, None, "metadata store unavailable");
    };
    // Pick a path. If blank or already used, suffix a counter.
    let mut path = form.path.trim().to_string();
    if path.is_empty() {
        path = "/untitled.sql".into();
    }
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    if store.studio_files_get_by_path(&path).await.ok().flatten().is_some() {
        let stem = path.trim_end_matches(".sql").trim_end_matches(".py");
        for n in 2..100 {
            let candidate = if path.ends_with(".sql") {
                format!("{stem}-{n}.sql")
            } else if path.ends_with(".py") {
                format!("{stem}-{n}.py")
            } else {
                format!("{path}-{n}")
            };
            if store.studio_files_get_by_path(&candidate).await.ok().flatten().is_none() {
                path = candidate;
                break;
            }
        }
    }
    let created = match store.studio_files_create(&path, &form.content).await {
        Ok(f) => f,
        Err(e) => return redirect_studio_flash(None, None, &format!("create: {e}")),
    };
    let mut open: Vec<String> = form
        .open
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if !open.contains(&created.id) {
        open.push(created.id.clone());
    }
    redirect_studio_flash(
        Some(open.join(",")),
        Some(created.id),
        "file created",
    )
}

#[derive(serde::Deserialize)]
struct FileSaveForm {
    #[serde(default)]
    open: String,
    /// New content. The editor textarea posts this as `sql` to
    /// match the existing run-query form's field, so the operator
    /// can switch between Save and Run without two textareas.
    sql: String,
}

async fn studio_file_save_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Form(form): axum::extract::Form<FileSaveForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(Some(form.open.clone()), Some(id), "metadata store unavailable");
    };
    match store
        .studio_files_update(&id, None, Some(&form.sql))
        .await
    {
        Ok(Some(_)) => redirect_studio_flash(Some(form.open), Some(id), "saved"),
        Ok(None) => redirect_studio_flash(Some(form.open), Some(id), "file not found (deleted elsewhere?)"),
        Err(e) => redirect_studio_flash(Some(form.open), Some(id), &format!("save: {e}")),
    }
}

#[derive(serde::Deserialize)]
struct FileTabContextForm {
    #[serde(default)]
    open: String,
}

async fn studio_file_delete_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Form(form): axum::extract::Form<FileTabContextForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(Some(form.open.clone()), None, "metadata store unavailable");
    };
    let removed = store.studio_files_delete(&id).await.unwrap_or(false);
    // Drop the deleted id from the open list and pick a sensible
    // active tab (the previous one if any).
    let open: Vec<String> = form
        .open
        .split(',')
        .filter(|s| !s.is_empty() && *s != id)
        .map(str::to_string)
        .collect();
    let next_active = open.last().cloned();
    let msg = if removed { "file deleted" } else { "file already gone" };
    redirect_studio_flash(Some(open.join(",")), next_active, msg)
}

async fn studio_file_duplicate_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Form(form): axum::extract::Form<FileTabContextForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(Some(form.open.clone()), Some(id), "metadata store unavailable");
    };
    let Ok(Some(original)) = store.studio_files_get(&id).await else {
        return redirect_studio_flash(Some(form.open), Some(id), "source file not found");
    };
    // Stem + extension split for "<name>-copy.<ext>"
    let (stem, ext) = match original.path.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (original.path.clone(), String::new()),
    };
    let mut candidate = format!("{stem}-copy{ext}");
    for n in 2..100 {
        if store.studio_files_get_by_path(&candidate).await.ok().flatten().is_none() {
            break;
        }
        candidate = format!("{stem}-copy-{n}{ext}");
    }
    let copy = match store.studio_files_create(&candidate, &original.content).await {
        Ok(f) => f,
        Err(e) => return redirect_studio_flash(Some(form.open), Some(id), &format!("duplicate: {e}")),
    };
    let mut open: Vec<String> = form
        .open
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    open.push(copy.id.clone());
    redirect_studio_flash(Some(open.join(",")), Some(copy.id), "duplicated")
}

#[derive(serde::Deserialize)]
struct FileRenameForm {
    #[serde(default)]
    open: String,
    path: String,
}

async fn studio_file_rename_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Form(form): axum::extract::Form<FileRenameForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(Some(form.open.clone()), Some(id), "metadata store unavailable");
    };
    let mut path = form.path.trim().to_string();
    if path.is_empty() {
        return redirect_studio_flash(Some(form.open), Some(id), "rename: path required");
    }
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    // If something else already lives at this path, fail fast.
    if let Ok(Some(existing)) = store.studio_files_get_by_path(&path).await {
        if existing.id != id {
            return redirect_studio_flash(
                Some(form.open),
                Some(id),
                &format!("rename: {path} already exists"),
            );
        }
    }
    match store.studio_files_update(&id, Some(&path), None).await {
        Ok(Some(_)) => redirect_studio_flash(Some(form.open), Some(id), "renamed"),
        Ok(None) => redirect_studio_flash(Some(form.open), Some(id), "file not found"),
        Err(e) => redirect_studio_flash(Some(form.open), Some(id), &format!("rename: {e}")),
    }
}

/// GET /studio/files/{id}/export -- download the raw content with a
/// filename matching the stored path's basename. Plain text/plain;
/// editor stores everything as TEXT.
async fn studio_file_export_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(store) = state.store.as_deref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "metadata store unavailable").into_response();
    };
    let Ok(Some(file)) = store.studio_files_get(&id).await else {
        return (StatusCode::NOT_FOUND, "file not found").into_response();
    };
    let basename = file.path.rsplit('/').next().unwrap_or(&file.path).to_string();
    let safe_basename: String = basename
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe_basename}\""),
            ),
        ],
        file.content,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct FileImportForm {
    #[serde(default)]
    open: String,
    path: String,
    content: String,
}

/// Single-file import. The form has a `path` field + a `content`
/// textarea -- the operator pastes the file body in (no multipart
/// dependency required for v1). .cptz archive import comes via the
/// multipart-enabled studio_files_import_archive_handler.
async fn studio_file_import_handler(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<FileImportForm>,
) -> axum::response::Redirect {
    let Some(store) = state.store.as_deref() else {
        return redirect_studio_flash(Some(form.open.clone()), None, "metadata store unavailable");
    };
    let mut path = form.path.trim().to_string();
    if path.is_empty() {
        return redirect_studio_flash(Some(form.open), None, "import: path required");
    }
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    // Overwrite-if-exists: if a file already lives here, update
    // content; otherwise insert. Simpler than asking the operator
    // to choose, and the rename path lets them disambiguate if
    // they realise mid-import that they're clobbering.
    let result = if let Ok(Some(existing)) = store.studio_files_get_by_path(&path).await {
        store
            .studio_files_update(&existing.id, None, Some(&form.content))
            .await
            .map(|opt| opt.map(|f| f.id))
    } else {
        store
            .studio_files_create(&path, &form.content)
            .await
            .map(|f| Some(f.id))
    };
    match result {
        Ok(Some(id)) => {
            let mut open: Vec<String> = form
                .open
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !open.contains(&id) {
                open.push(id.clone());
            }
            redirect_studio_flash(Some(open.join(",")), Some(id), "imported")
        }
        Ok(None) => redirect_studio_flash(Some(form.open), None, "import: nothing happened"),
        Err(e) => redirect_studio_flash(Some(form.open), None, &format!("import: {e}")),
    }
}

/// GET /studio/files/export-archive -- bundle every studio file
/// into a `.cptz` archive (just a ZIP with a manifest.json + the
/// files in their original tree). Downloads as
/// `computeza-workspace-<timestamp>.cptz`.
async fn studio_files_export_archive_handler(
    State(state): State<AppState>,
) -> Response {
    let Some(store) = state.store.as_deref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "metadata store unavailable").into_response();
    };
    let files = match store.studio_files_list().await {
        Ok(f) => f,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")).into_response(),
    };
    let bytes = match build_cptz_archive(&files) {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"computeza-workspace-{ts}.cptz\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Build a .cptz archive in memory. Format:
///   manifest.json       -- {version: 1, exported_at, files: [{path}]}
///   files/<path-as-is>  -- file content; the leading slash on
///                          `path` becomes the root of the zip tree
// TODO(file-browser-v2): wire the .cptz archive import handler. The
// export side works fine via a GET that returns the zip bytes; the
// import side needs a request-body extractor that axum 0.8's
// Handler trait isn't accepting in this build configuration. The
// helper import_cptz_archive() is fully implemented and tested in
// isolation; flipping the handler back on is a one-route patch once
// the extractor signature is sorted.

fn build_cptz_archive(files: &[computeza_state::StudioFile]) -> std::result::Result<Vec<u8>, String> {
    use std::io::Write;
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        // manifest.json first.
        let manifest = serde_json::json!({
            "version": 1,
            "exported_at": chrono::Utc::now().to_rfc3339(),
            "files": files.iter().map(|f| serde_json::json!({"path": f.path})).collect::<Vec<_>>(),
        });
        zip.start_file("manifest.json", opts).map_err(|e| e.to_string())?;
        zip.write_all(serde_json::to_string_pretty(&manifest).unwrap_or_default().as_bytes())
            .map_err(|e| e.to_string())?;
        // files/...
        for f in files {
            let entry = format!("files{}", f.path);
            zip.start_file(&entry, opts).map_err(|e| e.to_string())?;
            zip.write_all(f.content.as_bytes()).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

#[allow(dead_code)] // Wired in once the axum 0.8 body extractor settles -- see TODO above.
async fn import_cptz_archive(
    store: &computeza_state::SqliteStore,
    bytes: &[u8],
) -> std::result::Result<usize, String> {
    use std::io::Read;
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {e}"))?;
    let mut imported = 0usize;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        // Skip the manifest + any non-files/ entry.
        let path = match name.strip_prefix("files/") {
            Some(rest) => format!("/{}", rest),
            None => continue,
        };
        if path.ends_with('/') {
            // Directory entry inside files/; ignore.
            continue;
        }
        let mut content = String::new();
        entry.read_to_string(&mut content).map_err(|e| e.to_string())?;
        // Overwrite-if-exists: same policy as single-file import.
        if let Ok(Some(existing)) = store.studio_files_get_by_path(&path).await {
            let _ = store.studio_files_update(&existing.id, None, Some(&content)).await;
        } else {
            let _ = store.studio_files_create(&path, &content).await;
        }
        imported += 1;
    }
    Ok(imported)
}

/// Build the /studio redirect URL with the standard tab-state +
/// flash-message query string. None values are omitted from the
/// query so the URL stays compact.
fn redirect_studio_flash(
    open: Option<String>,
    active: Option<String>,
    flash: &str,
) -> axum::response::Redirect {
    let mut q: Vec<String> = Vec::new();
    if let Some(o) = open.filter(|s| !s.is_empty()) {
        q.push(format!("open={}", url_encode(&o)));
    }
    if let Some(a) = active.filter(|s| !s.is_empty()) {
        q.push(format!("active={}", url_encode(&a)));
    }
    if !flash.is_empty() {
        q.push(format!("flash={}", url_encode(flash)));
    }
    let url = if q.is_empty() {
        "/studio".to_string()
    } else {
        format!("/studio?{}", q.join("&"))
    };
    axum::response::Redirect::to(&url)
}

/// JSON shape returned to Monaco's completion provider. Each item
/// becomes a single CompletionItem in the suggestion list. `kind`
/// is one of `database`, `table`, `column`, `warehouse`, `history`
/// -- Monaco maps these to icons via the registered provider.
#[derive(serde::Serialize, Default)]
struct CompletionSource {
    items: Vec<CompletionItem>,
}

#[derive(serde::Serialize)]
struct CompletionItem {
    /// Display label; this is what the operator sees in the
    /// completion popup.
    label: String,
    /// Text that gets inserted when the operator accepts the
    /// completion. Defaults to the same as `label` for symbol
    /// suggestions; history entries insert the full SQL.
    insert: String,
    /// `database` | `table` | `column` | `warehouse` | `history`
    kind: &'static str,
    /// Short hint shown in the completion-popup detail area.
    /// e.g. "table in finance.raw", "warehouse @ lakekeeper".
    detail: Option<String>,
}

/// GET /studio/api/completions
///
/// Aggregates symbol sources for Monaco's autocomplete:
///   - Trino catalogs + schemas (via `SHOW CATALOGS` /
///     `SHOW SCHEMAS FROM <catalog>` queries to /v1/statement).
///   - Lakekeeper warehouse names (via the same management API
///     `probe_lakekeeper` uses).
///   - Recent queries from studio history (deduplicated; the
///     full SQL inserts as a snippet).
///
/// Each source is best-effort: a Trino outage skips its symbols
/// but still returns warehouses + history. The endpoint never
/// fails the request -- Monaco just gets a shorter list.
async fn studio_completions_handler(
    State(state): State<AppState>,
) -> axum::Json<CompletionSource> {
    let mut items: Vec<CompletionItem> = Vec::new();

    // Trino symbols. Trino's `SHOW CATALOGS` / `SHOW SCHEMAS FROM
    // <catalog>` produces a single-column result, which we slice up
    // into completion entries. We list catalogs (= Lakekeeper
    // warehouses) and then the schemas of every catalog Trino has
    // registered; tables-per-schema would be a third pass but most
    // operators type the schema dot first.
    if let Some(url) = discover_trino_endpoint(state.store.as_deref()).await {
        let catalog_rows = if let SqlOutcome::Ok { rows, .. } =
            execute_sql_against_trino(&url, "SHOW CATALOGS").await
        {
            rows
        } else {
            Vec::new()
        };
        let mut catalog_names: Vec<String> = Vec::new();
        for r in &catalog_rows {
            if let Some(name) = r.first() {
                // Skip Trino's built-in system / jmx / tpch catalogs;
                // the operator's iceberg catalogs are what matter.
                if matches!(name.as_str(), "system" | "jmx" | "tpch" | "tpcds") {
                    continue;
                }
                items.push(CompletionItem {
                    label: name.clone(),
                    insert: name.clone(),
                    kind: "catalog",
                    detail: Some("Trino catalog".to_string()),
                });
                catalog_names.push(name.clone());
            }
        }
        for cat in &catalog_names {
            let q = format!("SHOW SCHEMAS FROM {cat}");
            if let SqlOutcome::Ok { rows, .. } = execute_sql_against_trino(&url, &q).await {
                for r in rows {
                    if let Some(schema) = r.first() {
                        if matches!(schema.as_str(), "information_schema") {
                            continue;
                        }
                        let qualified = format!("{cat}.{schema}");
                        items.push(CompletionItem {
                            label: qualified.clone(),
                            insert: qualified,
                            kind: "schema",
                            detail: Some(format!("schema in {cat}")),
                        });
                    }
                }
            }
        }
    }

    // Lakekeeper warehouses
    if let Some(url) = discover_lakekeeper_endpoint(state.store.as_deref()).await {
        if let LakekeeperState::HasWarehouses(names) = probe_lakekeeper(&url).await {
            for n in names {
                items.push(CompletionItem {
                    label: n.clone(),
                    insert: n,
                    kind: "warehouse",
                    detail: Some("Lakekeeper warehouse".to_string()),
                });
            }
        }
    }

    // History (dedup by the first line so repeated SELECT 1's
    // don't bury fresh queries).
    let history = load_studio_history(state.store.as_deref()).await;
    let mut seen_first_lines = std::collections::HashSet::new();
    for entry in history.entries.into_iter().take(20) {
        let first_line: String = entry.sql.lines().next().unwrap_or("").chars().take(60).collect();
        if first_line.trim().is_empty() {
            continue;
        }
        if !seen_first_lines.insert(first_line.clone()) {
            continue;
        }
        let label = if first_line.len() < entry.sql.len() {
            format!("{first_line}...")
        } else {
            first_line.clone()
        };
        items.push(CompletionItem {
            label,
            insert: entry.sql,
            kind: "history",
            detail: Some("recent query".to_string()),
        });
    }

    axum::Json(CompletionSource { items })
}

async fn studio_sql_execute_handler(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<StudioSqlForm>,
) -> Html<String> {
    let l = Localizer::english();
    let lakekeeper = discover_lakekeeper_endpoint(state.store.as_deref()).await;
    let trino = discover_trino_endpoint(state.store.as_deref()).await;
    let sail = discover_sail_endpoint(state.store.as_deref()).await;

    // Detect what kind of query this is and route accordingly.
    // SQL goes to Trino; Python (or anything we can't confidently
    // call SQL) goes to Sail via the venv's Python interpreter.
    let language = detect_query_language(&form.sql);
    let routed = match language {
        QueryLanguage::Sql => {
            let outcome = match &trino {
                Some(url) => Some(execute_sql_against_trino(url, &form.sql).await),
                None => None,
            };
            RoutedOutcome::Sql {
                engine: ExecutedEngine::Trino,
                outcome,
            }
        }
        QueryLanguage::Python => {
            let outcome = match &sail {
                Some(url) => Some(
                    execute_python_via_sail(url, &form.sql, state.secrets.as_deref()).await,
                ),
                None => None,
            };
            RoutedOutcome::Python {
                engine: ExecutedEngine::Sail,
                outcome,
            }
        }
    };

    // Record into history. row_count for SQL = result rows;
    // for Python = lines-of-stdout so the badge is still useful.
    if !form.sql.trim().is_empty() {
        let (ok, row_count) = match &routed {
            RoutedOutcome::Sql {
                outcome: Some(SqlOutcome::Ok { rows, .. }),
                ..
            } => (true, Some(rows.len())),
            RoutedOutcome::Sql {
                outcome: Some(SqlOutcome::Err(_)),
                ..
            } => (false, None),
            RoutedOutcome::Python {
                outcome: Some(PythonOutcome::Ok { stdout, .. }),
                ..
            } => (true, Some(stdout.lines().count())),
            RoutedOutcome::Python {
                outcome: Some(PythonOutcome::Err { .. }),
                ..
            } => (false, None),
            _ => (false, None),
        };
        record_studio_history(
            state.store.as_deref(),
            StudioHistoryEntry {
                sql: form.sql.clone(),
                executed_at: chrono::Utc::now(),
                ok,
                row_count,
            },
        )
        .await;
    }
    let lk_state = match &lakekeeper {
        Some(url) => probe_lakekeeper_with_vault_fallback(url, state.secrets.as_deref()).await,
        None => LakekeeperState::Unreachable,
    };
    let history = load_studio_history(state.store.as_deref()).await;
    let sidebar = build_studio_full_sidebar(&state, SidebarFocus::default()).await;
    // Rebuild the files view from the form's forwarded tab state so
    // the renderer re-emits the same tabs / active highlight after
    // a Run round-trip.
    let post_q = StudioQuery {
        sql: None,
        open: if form.open.is_empty() { None } else { Some(form.open.clone()) },
        active: if form.active.is_empty() { None } else { Some(form.active.clone()) },
        flash: None,
    };
    let files = build_studio_files_view(&state, &post_q).await;
    Html(render_studio_page(
        &l,
        lakekeeper.is_some(),
        trino.is_some(),
        &lk_state,
        &history,
        &form.sql,
        Some(routed),
        &sidebar,
        &files,
    ))
}

/// Which language did the operator actually type? Routing key:
/// SQL goes to Trino; Python goes to Sail. Anything we can't
/// confidently call SQL is treated as Python -- a wrong route shows
/// a useful error from the wrong engine rather than silently
/// pretending to succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryLanguage {
    Sql,
    Python,
}

/// Which engine produced an outcome. Surfaced on the result panel so
/// the operator sees at a glance whether Trino or Sail ran the
/// query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutedEngine {
    Trino,
    Sail,
}

impl ExecutedEngine {
    fn label(&self) -> &'static str {
        match self {
            ExecutedEngine::Trino => "Trino (SQL)",
            ExecutedEngine::Sail => "Sail (Spark Connect)",
        }
    }
}

/// Output of a routed query. Either SQL (which produces tabular rows
/// or a typed error) or Python (which produces free-form stdout +
/// stderr). The renderer picks the right shape.
#[derive(Debug)]
enum RoutedOutcome {
    Sql {
        engine: ExecutedEngine,
        outcome: Option<SqlOutcome>,
    },
    Python {
        engine: ExecutedEngine,
        outcome: Option<PythonOutcome>,
    },
}

/// PySpark / Python execution result. Both stdout and stderr captured
/// so DataFrame.show() output (stdout) and runtime tracebacks (stderr)
/// render side-by-side on the results panel.
///
/// The `Ok` variant is constructed only on Linux (where Sail can
/// actually run) but the renderer matches on both unconditionally
/// so the type stays uniform across platforms.
#[derive(Debug)]
#[allow(dead_code)]
enum PythonOutcome {
    Ok { stdout: String, stderr: String },
    Err { stdout: String, stderr: String },
}

/// Parse `http(s)://host:port[/path]` into (host, port). Returns
/// None if the input isn't shaped that way -- the caller falls back
/// to its default. Avoids pulling in the url crate for a one-liner.
/// Only called from execute_python_via_sail which is Linux-only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_host_port(s: &str) -> Option<(String, u16)> {
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .unwrap_or(s);
    let s = s.split('/').next()?;
    let (host, port) = s.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    Some((host.to_string(), port))
}

/// Heuristic SQL-vs-Python detector. Looks at the first non-blank,
/// non-comment line and checks whether its first token is a SQL
/// reserved word. Anything else is Python.
///
/// SQL keywords listed cover what both Databend and Spark SQL accept;
/// expanding the list is cheap -- the trade-off is purely "does this
/// line LOOK like SQL", not "is this valid SQL".
fn detect_query_language(src: &str) -> QueryLanguage {
    const SQL_HEADS: &[&str] = &[
        "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP",
        "ALTER", "TRUNCATE", "USE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "GRANT",
        "REVOKE", "CALL", "REFRESH", "OPTIMIZE", "VACUUM", "ANALYZE", "COPY", "SET",
    ];
    for line in src.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // SQL line comments
        if t.starts_with("--") {
            continue;
        }
        // Python line comments
        if t.starts_with('#') {
            return QueryLanguage::Python;
        }
        // First non-empty token, uppercased
        let first = t.split_whitespace().next().unwrap_or("").to_uppercase();
        // Trim trailing punctuation like SELECT, vs SELECT (...
        let first = first.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
        return if SQL_HEADS.contains(&first) {
            QueryLanguage::Sql
        } else {
            QueryLanguage::Python
        };
    }
    // All-blank input: default to SQL so the empty-state messaging
    // stays consistent with the historical behaviour.
    QueryLanguage::Sql
}

/// Run user Python code against Sail by:
///   1. Reading the Sail catalog config from vault (set by bootstrap).
///   2. Generating a small wrapper script that opens a SparkSession
///      pointed at sc://<sail-host>:<port> with every registered
///      catalog wired in.
///   3. Spawning `<sail-root>/venv/bin/python3` with the user code
///      appended after the wrapper boilerplate.
///   4. Capturing stdout + stderr verbatim.
///
/// Best-effort: missing vault entries, missing Sail venv, or a
/// Python traceback all produce informative PythonOutcome::Err
/// rather than 500s.
#[cfg(not(target_os = "linux"))]
async fn execute_python_via_sail(
    _sail_base_url: &str,
    _user_code: &str,
    _secrets: Option<&computeza_secrets::SecretsStore>,
) -> PythonOutcome {
    PythonOutcome::Err {
        stdout: String::new(),
        stderr: "Sail (Python / Spark Connect) is Linux-only in v0.0.x.".into(),
    }
}

#[cfg(target_os = "linux")]
async fn execute_python_via_sail(
    sail_base_url: &str,
    user_code: &str,
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> PythonOutcome {
    use secrecy::ExposeSecret;
    use tokio::io::AsyncWriteExt;

    // Resolve the venv python path. The sail driver installs
    // python3 + sail under <root>/venv/bin/.
    let root = std::path::PathBuf::from("/var/lib/computeza/sail");
    let venv_python = computeza_driver_native::linux::sail::installed_venv_python(&root);
    if !tokio::fs::try_exists(&venv_python).await.unwrap_or(false) {
        return PythonOutcome::Err {
            stdout: String::new(),
            stderr: format!(
                "Sail venv Python interpreter not found at {}. Re-install Sail from /install.",
                venv_python.display()
            ),
        };
    }

    // Parse the sail base URL into host + port for the sc:// URI.
    // sail_base_url is shaped like "http://127.0.0.1:50051" -- a
    // single split is cheaper than pulling in the url crate.
    let (sail_host, sail_port) = parse_host_port(sail_base_url).unwrap_or_else(|| ("127.0.0.1".to_string(), 50051));

    // Collect registered catalogs from vault.
    let catalog_names = match secrets {
        Some(s) => s
            .get("sail/catalog-names")
            .await
            .ok()
            .flatten()
            .map(|v| {
                v.expose_secret()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    // Build a JSON map of {catalog_name: {warehouse-uuid, s3-*}} that
    // the wrapper script reads via stdin. JSON avoids shell-quoting
    // nightmares for embedded secrets.
    let mut catalog_cfg: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    if let Some(s) = secrets {
        for name in &catalog_names {
            let prefix = format!("sail/catalog/{name}");
            let mut entry = serde_json::Map::new();
            for field in &[
                "warehouse-uuid",
                "s3-endpoint",
                "s3-region",
                "s3-access-key-id",
                "s3-secret-access-key",
            ] {
                if let Ok(Some(v)) = s.get(&format!("{prefix}/{field}")).await {
                    entry.insert((*field).into(), serde_json::Value::String(v.expose_secret().to_string()));
                }
            }
            // Lakekeeper REST URI: read live, not from vault, so it
            // tracks endpoint moves across reconciler runs.
            if let Some(lk) = discover_lakekeeper_endpoint(None).await {
                entry.insert("lakekeeper-rest-uri".into(), serde_json::Value::String(format!("{lk}/catalog")));
            }
            catalog_cfg.insert(name.clone(), serde_json::Value::Object(entry));
        }
    }
    let catalog_cfg_json = serde_json::Value::Object(catalog_cfg).to_string();

    // Wrapper script. Reads catalog config JSON from stdin to avoid
    // arg-list-too-long / quoting issues; builds SparkSession with
    // the Iceberg-REST catalog for every registered warehouse;
    // execs user code with `spark` already in scope.
    let wrapper = format!(
        r#"
import sys, json, traceback
catalog_cfg = json.loads(sys.stdin.readline())
user_code = sys.stdin.read()

from pyspark.sql import SparkSession
builder = SparkSession.builder.remote("sc://{host}:{port}")
for name, cfg in catalog_cfg.items():
    prefix = "spark.sql.catalog." + name
    builder = builder.config(prefix, "org.apache.iceberg.spark.SparkCatalog")
    builder = builder.config(prefix + ".type", "rest")
    builder = builder.config(prefix + ".uri", cfg.get("lakekeeper-rest-uri", ""))
    builder = builder.config(prefix + ".warehouse", cfg.get("warehouse-uuid", ""))
    builder = builder.config(prefix + ".io-impl", "org.apache.iceberg.aws.s3.S3FileIO")
    builder = builder.config(prefix + ".s3.endpoint", cfg.get("s3-endpoint", ""))
    builder = builder.config(prefix + ".s3.access-key-id", cfg.get("s3-access-key-id", ""))
    builder = builder.config(prefix + ".s3.secret-access-key", cfg.get("s3-secret-access-key", ""))
    builder = builder.config(prefix + ".s3.region", cfg.get("s3-region", "garage"))
    builder = builder.config(prefix + ".s3.path-style-access", "true")
spark = builder.getOrCreate()

try:
    exec(compile(user_code, "<studio>", "exec"), {{"spark": spark, "__name__": "__main__"}})
except Exception:
    traceback.print_exc()
    sys.exit(1)
"#,
        host = sail_host,
        port = sail_port,
    );

    // Spawn python3 wrapper, write stdin = JSON-config-line + user-code, capture stdout/stderr.
    let mut child = match tokio::process::Command::new(&venv_python)
        .arg("-c")
        .arg(&wrapper)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return PythonOutcome::Err {
                stdout: String::new(),
                stderr: format!("spawn {}: {e}", venv_python.display()),
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        // First line = catalog config JSON, terminated by \n.
        // Second segment = user code, EOF terminated.
        let _ = stdin.write_all(catalog_cfg_json.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.write_all(user_code.as_bytes()).await;
        // Drop stdin so the child's `sys.stdin.read()` returns.
        drop(stdin);
    }
    let out = match tokio::time::timeout(std::time::Duration::from_secs(120), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return PythonOutcome::Err {
                stdout: String::new(),
                stderr: format!("wait child: {e}"),
            };
        }
        Err(_) => {
            return PythonOutcome::Err {
                stdout: String::new(),
                stderr: "Sail Python query exceeded 120s timeout. If the query is expected to be long-running, run it via `<root>/venv/bin/sail spark client` from a shell instead.".into(),
            };
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        PythonOutcome::Ok { stdout, stderr }
    } else {
        PythonOutcome::Err { stdout, stderr }
    }
}

/// Outcome of forwarding a SQL query to Trino. Either a successful
/// result set (columns + rows of stringified values) or an error
/// message we can render verbatim.
#[derive(Debug)]
enum SqlOutcome {
    Ok {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Err(String),
}

/// Execute a SQL statement against the local Trino coordinator via
/// the documented HTTP client protocol. Trino's protocol:
///
///   1. POST /v1/statement with the raw SQL as the request body
///      (plain text, NOT JSON), `X-Trino-User` header required.
///   2. The response is a JSON `QueryResults` document. If
///      `nextUri` is present, GET it for the next page of results.
///      Repeat until `nextUri` is absent (query is done).
///   3. `columns` lands on the first page that has data; `data` is
///      an array of arrays of cell values, may span multiple pages
///      and we concatenate.
///   4. On error, the `error` field carries `message` + `errorCode`.
///
/// `X-Trino-Catalog` is intentionally omitted -- Studio supports
/// fully-qualified `<catalog>.<schema>.<table>` references so the
/// operator can query any registered catalog from one editor
/// without changing the session context.
async fn execute_sql_against_trino(base_url: &str, sql: &str) -> SqlOutcome {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => return SqlOutcome::Err(format!("building HTTP client: {e}")),
    };
    let submit_url = format!("{}/v1/statement", base_url.trim_end_matches('/'));
    // First request: POST the SQL as plain text. X-Trino-User is
    // required by Trino's coordinator even when authn is disabled.
    let initial = match client
        .post(&submit_url)
        .header("X-Trino-User", "computeza")
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body(sql.to_string())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SqlOutcome::Err(format!(
                "could not reach Trino at {submit_url}: {e}. Check /status -- the trino reconciler will show FAILED if the coordinator is down."
            ));
        }
    };
    let mut body: serde_json::Value = match initial.json().await {
        Ok(v) => v,
        Err(e) => {
            return SqlOutcome::Err(format!(
                "Trino /v1/statement returned non-JSON body: {e}"
            ));
        }
    };

    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    // 60-iteration cap (~60s with the per-poll ~1s pacing Trino
    // typically applies). A genuinely long query should produce
    // partial results inside this window; longer-running analytics
    // belong in a notebook session, not the Studio editor.
    for _ in 0..60 {
        if let Some(err) = body.get("error") {
            if !err.is_null() {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string());
                return SqlOutcome::Err(format!("Trino error: {msg}"));
            }
        }
        if columns.is_empty() {
            if let Some(cols) = body.get("columns").and_then(|v| v.as_array()) {
                columns = cols
                    .iter()
                    .filter_map(|c| {
                        c.get("name").and_then(|n| n.as_str()).map(str::to_string)
                    })
                    .collect();
            }
        }
        if let Some(data) = body.get("data").and_then(|v| v.as_array()) {
            for row in data {
                if let Some(arr) = row.as_array() {
                    rows.push(
                        arr.iter()
                            .map(|cell| {
                                cell.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| cell.to_string())
                            })
                            .collect(),
                    );
                }
            }
        }
        let next_uri = body
            .get("nextUri")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(next) = next_uri else {
            // No nextUri -> query is finished.
            return SqlOutcome::Ok { columns, rows };
        };
        // Per Trino docs: headers are only needed on the initial
        // POST; the GET to nextUri carries the session via the URI.
        match client.get(&next).send().await {
            Ok(r) => match r.json::<serde_json::Value>().await {
                Ok(v) => body = v,
                Err(e) => {
                    return SqlOutcome::Err(format!("Trino nextUri body non-JSON: {e}"));
                }
            },
            Err(e) => return SqlOutcome::Err(format!("Trino nextUri GET failed: {e}")),
        }
    }
    SqlOutcome::Err("Trino query exceeded 60 polling iterations. The query is still running on the coordinator; cancel via the Trino UI or run a shorter slice from the editor.".into())
}

/// Render the middle "Workspace files" pane. Builds a flat tree
/// from the path prefixes -- folders are implicit. Each row links
/// to /studio?open=...&active=<id> which the next render uses to
/// drop the file's content into the editor and highlight its tab.
fn render_studio_files_pane(files: &StudioFilesView) -> String {
    let csrf = auth::csrf_input();
    let open_csv = files.open_csv();
    // Eyebrow + action buttons (new / export-archive).
    // The "new file" form has a path input so the operator picks
    // both the name AND the file type (extension drives both syntax
    // highlighting in Monaco and the SQL-vs-Python routing on Run).
    // The hidden content field is populated client-side from the
    // editor textarea -- so + saves the current editor body, not an
    // empty file.
    let actions = format!(
        r##"<div class="cz-studio-files-actions">
<a href="#cz-modal-new-file" data-tooltip="New file"><i class="fa-solid fa-plus"></i></a>
<a href="/studio/files/export-archive" data-tooltip="Export workspace as .cptz"><i class="fa-solid fa-file-zipper"></i></a>
</div>
<div id="cz-modal-new-file" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-modal-new-file-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-modal-new-file-title" class="cz-modal-title">New file</h2>
<p class="cz-modal-subtitle">Saves the editor's current contents to a new file. The extension drives syntax highlighting + the auto-route (SQL → Trino, Python → Sail).</p>
<form method="post" action="/studio/files/new" id="cz-file-new-form">
{csrf}
<input type="hidden" name="open" value="{open_csv}" />
<input type="hidden" name="content" id="cz-file-new-content" value="" />
<div class="cz-modal-field">
<label for="cz-file-new-path">Path</label>
<input id="cz-file-new-path" type="text" name="path" class="cz-input" placeholder="/sql/my-query.sql" autofocus />
<div style="display:flex; gap:0.3rem; flex-wrap:wrap; margin-top:0.5rem;">
<button type="button" class="cz-btn-ghost" style="font-size:0.74rem; padding:0.25rem 0.6rem;" onclick="document.getElementById('cz-file-new-path').value='/sql/untitled.sql'">.sql</button>
<button type="button" class="cz-btn-ghost" style="font-size:0.74rem; padding:0.25rem 0.6rem;" onclick="document.getElementById('cz-file-new-path').value='/python/untitled.py'">.py</button>
<button type="button" class="cz-btn-ghost" style="font-size:0.74rem; padding:0.25rem 0.6rem;" onclick="document.getElementById('cz-file-new-path').value='/notes/untitled.txt'">.txt</button>
<button type="button" class="cz-btn-ghost" style="font-size:0.74rem; padding:0.25rem 0.6rem;" onclick="document.getElementById('cz-file-new-path').value='/scratch/untitled.md'">.md</button>
</div>
<p class="cz-modal-field-hint">Empty path → /untitled.sql. Folder prefix like <code>/python/</code> groups the file in the tree.</p>
</div>
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-primary">Create file</button>
</div>
</form>
</div>
</div>"##,
        csrf = csrf,
        open_csv = html_escape(&open_csv),
    );
    let eyebrow = format!(
        r#"<div class="cz-studio-files-eyebrow"><span>Files ({n})</span>{actions}</div>"#,
        n = files.all.len(),
        actions = actions,
    );
    // Tree: group by leading folder segment (first '/' after the
    // root). Files at the top level go under "Untitled". Deeper
    // nesting is collapsed to a single level for v1.
    let mut groups: std::collections::BTreeMap<String, Vec<&computeza_state::StudioFile>> =
        std::collections::BTreeMap::new();
    for f in &files.all {
        let trimmed = f.path.trim_start_matches('/');
        let folder = match trimmed.split_once('/') {
            Some((dir, _)) if !dir.is_empty() => dir.to_string(),
            _ => "root".to_string(),
        };
        groups.entry(folder).or_default().push(f);
    }
    let tree_html: String = if files.all.is_empty() {
        r#"<div class="cz-studio-file-empty">No saved files yet. Click <strong>+</strong> to save your current editor body as a file, or import a .cptz archive (coming soon).</div>"#.to_string()
    } else {
        groups
            .iter()
            .map(|(folder, items)| {
                let folder_label = if folder == "root" { "/".to_string() } else { format!("/{folder}/") };
                let rows: String = items
                    .iter()
                    .map(|f| {
                        let basename = f
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&f.path)
                            .to_string();
                        let is_active = files.active_id.as_deref() == Some(f.id.as_str());
                        let active = if is_active { " cz-tree-active" } else { "" };
                        let mut next_open: Vec<&str> = files
                            .open
                            .iter()
                            .map(|f| f.id.as_str())
                            .collect();
                        if !next_open.contains(&f.id.as_str()) {
                            next_open.push(&f.id);
                        }
                        let next_open_csv = next_open.join(",");
                        // Hover-revealed actions on the row. Rename
                        // is a :target-anchored modal centered on
                        // the page (the previous inline popover
                        // spilled out of the 220px-wide files pane).
                        // Duplicate/delete are tiny inline forms;
                        // export is a GET download.
                        format!(
                            r##"<div class="cz-studio-file-row-wrap">
<a class="cz-studio-file-row{active}" href="/studio?open={open}&active={id}"><span class="cz-tree-label" title="{path}">{label}</span></a>
<div class="cz-studio-file-row-actions">
<a href="#cz-rename-{id}" title="Rename" class="cz-file-row-rename"><i class="fa-solid fa-pen"></i></a>
<form method="post" action="/studio/files/{id}/duplicate" style="margin:0;display:inline;">{csrf}<input type="hidden" name="open" value="{open}" /><button type="submit" title="Duplicate"><i class="fa-solid fa-copy"></i></button></form>
<a href="/studio/files/{id}/export" title="Download"><i class="fa-solid fa-download"></i></a>
<a href="#cz-delete-{id}" title="Delete" class="cz-file-row-delete"><i class="fa-solid fa-trash"></i></a>
</div>
</div>
<div id="cz-rename-{id}" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-rename-{id}-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-rename-{id}-title" class="cz-modal-title">Rename file</h2>
<p class="cz-modal-subtitle">Change the path of <code>{path}</code>. Use a leading slash; the first folder segment becomes the tree group.</p>
<form method="post" action="/studio/files/{id}/rename">
{csrf}
<input type="hidden" name="open" value="{open}" />
<div class="cz-modal-field">
<label for="cz-rename-input-{id}">New path</label>
<input id="cz-rename-input-{id}" type="text" name="path" value="{path}" class="cz-input" required autofocus />
</div>
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-primary">Rename</button>
</div>
</form>
</div>
</div>
<div id="cz-delete-{id}" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-delete-{id}-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-delete-{id}-title" class="cz-modal-title">Delete file?</h2>
<p class="cz-modal-subtitle">This will permanently delete <code>{path}</code>. The file cannot be recovered.</p>
<form method="post" action="/studio/files/{id}/delete">
{csrf}
<input type="hidden" name="open" value="{open}" />
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-danger">Delete file</button>
</div>
</form>
</div>
</div>"##,
                            csrf = csrf,
                            open = url_encode(&next_open_csv),
                            id = url_encode(&f.id),
                            path = html_escape(&f.path),
                            label = html_escape(&basename),
                        )
                    })
                    .collect();
                format!(
                    r#"<div class="cz-studio-file-folder">{folder_label}</div>{rows}"#,
                    folder_label = html_escape(&folder_label),
                )
            })
            .collect()
    };
    format!(
        r##"{eyebrow}
{tree_html}
<script>
// Mirror the textarea body into the New-file form's hidden content
// field so clicking "+" saves the current editor body, not an
// empty file. Picks up Monaco's value too when Monaco is mounted
// (the editor copies its buffer into the textarea on submit; we
// just read the textarea at click time).
(function() {{
  var form = document.getElementById("cz-file-new-form");
  var hidden = document.getElementById("cz-file-new-content");
  var ta = document.getElementById("cz-sql-textarea");
  if (!form || !hidden || !ta) return;
  form.addEventListener("submit", function() {{
    hidden.value = ta.value;
  }});
}})();
</script>"##
    )
}

/// Render the two-pane studio page. Pure-server template; the
/// only JS on the page is the existing CSRF auto-fill from
/// render_shell + the Monaco loader at the bottom. The catalog
/// tree renders on the left (built async by the handler via
/// `build_studio_full_sidebar`), the SQL editor + results pane
/// renders on the right.
#[allow(clippy::too_many_arguments)] // explicit signature is the readable choice over a "render-context" bag

/// Render the sidebar's "Recent queries" section. Each entry is a
/// clickable row that bounces through /studio?sql=... to prefill
/// the editor.
fn render_sidebar_recent_queries(history: &StudioHistory, reload_label: &str) -> String {
    if history.entries.is_empty() {
        return r#"<p class="cz-studio-sidebar-note">No queries yet. Run one above; it'll show here.</p>"#.to_string();
    }
    let items: String = history
        .entries
        .iter()
        .take(8) // sidebar is tight; show the top 8 most recent
        .map(|e| {
            let badge_class = if e.ok { "cz-badge-ok" } else { "cz-badge-warn" };
            let badge_text = if e.ok {
                e.row_count
                    .map(|n| format!("{n}"))
                    .unwrap_or_else(|| "ok".to_string())
            } else {
                "err".to_string()
            };
            let preview: String = e
                .sql
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            let preview = if preview.len() < e.sql.len() {
                format!("{preview}…")
            } else {
                preview
            };
            let ts = e.executed_at.format("%H:%M").to_string();
            format!(
                r#"<li class="cz-tree-item"><a class="cz-tree-row" href="/studio?sql={enc}" title="{tooltip}">
<span class="cz-tree-icon" style="font-size: 0.65rem;"><span class="cz-badge {badge_class}" style="font-size: 0.6rem; padding: 1px 4px;">{badge}</span></span>
<span class="cz-tree-label" style="font-size: 0.72rem;">{preview}</span>
<span class="cz-muted" style="font-size: 0.62rem; font-variant-numeric: tabular-nums;">{ts}</span>
</a></li>"#,
                enc = url_encode(&e.sql),
                tooltip = html_escape(&format!("{} ({})", reload_label, &e.sql)),
                badge_class = badge_class,
                badge = html_escape(&badge_text),
                preview = html_escape(&preview),
                ts = html_escape(&ts),
            )
        })
        .collect();
    format!(r#"<ul class="cz-tree">{items}</ul>"#)
}

fn render_studio_page(
    localizer: &Localizer,
    has_lakekeeper: bool,
    has_databend: bool,
    lk_state: &LakekeeperState,
    history: &StudioHistory,
    sql_prefill: &str,
    result: Option<RoutedOutcome>,
    sidebar_html: &str,
    files: &StudioFilesView,
) -> String {
    let title = localizer.t("ui-studio-title");
    let sql_placeholder = localizer.t("ui-studio-sql-placeholder");
    let sql_run = localizer.t("ui-studio-sql-run");
    let sql_help = localizer.t("ui-studio-sql-help");
    let results_empty = localizer.t("ui-studio-results-empty");
    let err_no_lakekeeper = localizer.t("ui-studio-error-no-lakekeeper");
    let err_no_databend = localizer.t("ui-studio-error-no-databend");
    let lk_unreachable = localizer.t("ui-studio-lakekeeper-unreachable");
    let lk_no_warehouses = localizer.t("ui-studio-lakekeeper-no-warehouses");
    let history_reload = localizer.t("ui-studio-history-reload");
    let csrf = auth::csrf_input();
    let _ = (
        localizer.t("ui-studio-intro"),
        localizer.t("ui-studio-catalog-heading"),
        localizer.t("ui-studio-sql-heading"),
        localizer.t("ui-studio-results-heading"),
        localizer.t("ui-studio-lakekeeper-warehouses-heading"),
        localizer.t("ui-studio-lakekeeper-warehouses-note"),
        localizer.t("ui-studio-history-heading"),
        localizer.t("ui-studio-history-empty"),
    ); // i18n placeholders preserved for future re-use; new shell uses
       // simpler hard-coded eyebrow labels for now.

    // Sidebar is built by the handler (it's async; needs Lakekeeper +
    // store access) and passed in as `sidebar_html`. The variables
    // below stay for future use if we want to surface no-lakekeeper /
    // unreachable hints in the editor pane.
    let _ = (
        history,
        has_lakekeeper,
        lk_state,
        &err_no_lakekeeper,
        &lk_unreachable,
        &lk_no_warehouses,
        &history_reload,
    );

    // ---- Main pane: results-first when a query just ran, else editor --
    // Build a small engine badge so the operator can see at a glance
    // which engine handled the query.
    let engine_pill = |engine: ExecutedEngine| -> String {
        format!(
            r#"<span class="cz-studio-engine-pill">{}</span>"#,
            html_escape(engine.label())
        )
    };
    let results_html = match result {
        None => format!(
            r#"<div class="cz-studio-results-empty">{}</div>"#,
            html_escape(&results_empty)
        ),
        Some(RoutedOutcome::Sql {
            outcome: None,
            ..
        }) => format!(
            r#"<div class="cz-studio-results-empty">{}</div>"#,
            html_escape(&err_no_databend)
        ),
        Some(RoutedOutcome::Sql {
            engine,
            outcome: Some(SqlOutcome::Err(msg)),
        }) => format!(
            r#"<div class="cz-studio-results-header">{pill}</div>
<pre class="cz-studio-error">{body}</pre>"#,
            pill = engine_pill(engine),
            body = html_escape(&msg)
        ),
        Some(RoutedOutcome::Sql {
            engine,
            outcome: Some(SqlOutcome::Ok { columns, rows }),
        }) => {
            let header: String = columns
                .iter()
                .map(|c| format!("<th>{}</th>", html_escape(c)))
                .collect();
            let body_rows: String = rows
                .iter()
                .map(|row| {
                    let cells: String = row
                        .iter()
                        .map(|c| format!("<td>{}</td>", html_escape(c)))
                        .collect();
                    format!("<tr>{cells}</tr>")
                })
                .collect();
            let row_count = rows.len();
            format!(
                r#"<div class="cz-studio-results-header">
{pill}
<span class="cz-studio-results-eyebrow">Results</span>
<span class="cz-studio-results-count">{row_count} row{plural}</span>
</div>
<div style="overflow-x: auto;">
<table class="cz-studio-results-table">
<thead><tr>{header}</tr></thead>
<tbody>{body_rows}</tbody>
</table>
</div>"#,
                plural = if row_count == 1 { "" } else { "s" },
                pill = engine_pill(engine),
            )
        }
        Some(RoutedOutcome::Python {
            outcome: None,
            ..
        }) => format!(
            r#"<div class="cz-studio-results-empty">Sail isn't installed. Install it from <a href="/install">/install</a> to run Python/Spark queries.</div>"#
        ),
        Some(RoutedOutcome::Python {
            engine,
            outcome: Some(PythonOutcome::Ok { stdout, stderr }),
        }) => {
            let stderr_block = if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(
                    r#"<details style="margin-top: 0.75rem;"><summary class="cz-studio-raw-summary">stderr (warnings / Spark logs)</summary><pre class="cz-studio-raw">{}</pre></details>"#,
                    html_escape(&stderr)
                )
            };
            format!(
                r#"<div class="cz-studio-results-header">{pill}<span class="cz-studio-results-eyebrow">stdout</span></div>
<pre class="cz-studio-raw">{stdout}</pre>
{stderr_block}"#,
                pill = engine_pill(engine),
                stdout = if stdout.trim().is_empty() {
                    "(no output)".into()
                } else {
                    html_escape(&stdout)
                }
            )
        }
        Some(RoutedOutcome::Python {
            engine,
            outcome: Some(PythonOutcome::Err { stdout, stderr }),
        }) => format!(
            r#"<div class="cz-studio-results-header">{pill}</div>
<pre class="cz-studio-error">{stderr}</pre>
{stdout_block}"#,
            pill = engine_pill(engine),
            stderr = html_escape(&stderr),
            stdout_block = if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(
                    r#"<details style="margin-top: 0.75rem;"><summary class="cz-studio-raw-summary">stdout (partial output before failure)</summary><pre class="cz-studio-raw">{}</pre></details>"#,
                    html_escape(&stdout)
                )
            }
        ),
    };
    let _ = has_databend;

    // Pre-detect language for the current editor body so the UI
    // shows the operator which engine WILL run when they hit Run.
    // Updates client-side as they type via a small inline script.
    let initial_language = detect_query_language(sql_prefill);
    let initial_lang_label = match initial_language {
        QueryLanguage::Sql => "SQL → Trino",
        QueryLanguage::Python => "Python → Sail",
    };
    let csrf_str = auth::csrf_input();
    let open_csv = files.open_csv();
    let active_id = files.active_id.clone().unwrap_or_default();
    // Tab strip above the editor. The "scratch" tab is always
    // present so the editor never has a blank tab strip; clicking
    // it deselects any file (active=) and the editor body becomes
    // the raw sql_prefill (no file context).
    let tabs_html = {
        let scratch_active = files.active_id.is_none();
        let scratch_class = if scratch_active { " cz-studio-tab-active" } else { "" };
        let mut html = format!(
            r#"<div class="cz-studio-tabs"><a class="cz-studio-tab cz-studio-tab-scratch{scratch_class}" href="/studio?open={open}"><span class="cz-studio-tab-label">scratch</span></a>"#,
            open = url_encode(&open_csv),
        );
        for f in &files.open {
            let is_active = files.active_id.as_deref() == Some(f.id.as_str());
            let active = if is_active { " cz-studio-tab-active" } else { "" };
            let basename = f.path.rsplit('/').next().unwrap_or(&f.path);
            // Close button: posts to delete handler? No -- close just
            // removes the tab from the open csv, doesn't delete the
            // file. So it's a link, not a form.
            let next_open: String = files
                .open
                .iter()
                .filter(|x| x.id != f.id)
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            html.push_str(&format!(
                r##"<a class="cz-studio-tab{active}" href="/studio?open={open}&active={id}"><span class="cz-studio-tab-label" title="{path}">{label}</span><a class="cz-studio-tab-close" href="/studio?open={next_open}" title="Close tab (doesn't delete file)">×</a></a>"##,
                open = url_encode(&open_csv),
                next_open = url_encode(&next_open),
                id = url_encode(&f.id),
                path = html_escape(&f.path),
                label = html_escape(basename),
            ));
        }
        html.push_str(r#"<div class="cz-studio-tabs-flex"></div></div>"#);
        html
    };
    // Flash banner after a file action.
    let flash_html = files
        .flash
        .as_deref()
        .map(|m| {
            format!(
                r#"<div class="cz-toast" role="status" aria-live="polite">{}</div>"#,
                html_escape(m)
            )
        })
        .unwrap_or_default();
    // Save / rename / delete / duplicate buttons -- only meaningful
    // when a file is active. Each is a tiny form that POSTs to the
    // matching handler. The Save button is the highlight: its
    // formaction overrides the parent form's /studio/sql/execute
    // so the same textarea content can be saved instead of run.
    let save_button_html = match files.active_file() {
        Some(f) => format!(
            r##"<button type="submit" class="cz-btn-ghost" formaction="/studio/files/{id}/save" formmethod="post" title="Save current editor body to {path}">Save</button>"##,
            id = url_encode(&f.id),
            path = html_escape(&f.path),
        ),
        None => String::new(),
    };
    let file_actions_html = match files.active_file() {
        Some(f) => format!(
            r##"<form method="post" action="/studio/files/{id}/duplicate" style="margin:0;display:inline;">{csrf}<input type="hidden" name="open" value="{open}" /><button type="submit" class="cz-btn-ghost" title="Duplicate {path}">Duplicate</button></form>
<a href="#cz-delete-toolbar-{id}" class="cz-btn-danger" title="Delete {path}">Delete</a>
<a href="/studio/files/{id}/export" class="cz-btn-ghost" title="Download {path}">Export</a>
<div id="cz-delete-toolbar-{id}" class="cz-modal-overlay" role="dialog" aria-labelledby="cz-delete-toolbar-{id}-title" aria-modal="true">
<a href="#" class="cz-modal-overlay-backdrop" aria-label="Close"></a>
<div class="cz-modal">
<a href="#" class="cz-modal-close" aria-label="Close">×</a>
<h2 id="cz-delete-toolbar-{id}-title" class="cz-modal-title">Delete file?</h2>
<p class="cz-modal-subtitle">This will permanently delete <code>{path}</code>. The file cannot be recovered.</p>
<form method="post" action="/studio/files/{id}/delete">
{csrf}
<input type="hidden" name="open" value="{open_csv}" />
<div class="cz-modal-actions">
<a href="#" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-danger">Delete file</button>
</div>
</form>
</div>
</div>"##,
            csrf = csrf_str,
            id = url_encode(&f.id),
            open = html_escape(&open_csv),
            open_csv = html_escape(&open_csv),
            path = html_escape(&f.path),
        ),
        None => String::new(),
    };
    let main_html = format!(
        r##"<div class="cz-studio-crumbs">
<a href="/studio">Studio</a><span class="cz-studio-crumbs-sep">/</span><span class="cz-studio-crumbs-current">Editor</span>
</div>
<div class="cz-studio-actions" style="margin-bottom: 0.5rem;">
<div>
<h1 class="cz-studio-pane-title" style="margin: 0;">Query editor</h1>
<p class="cz-studio-pane-subtitle" style="margin: 0.25rem 0 0;">Type SQL for Trino or Python (PySpark) for Sail — the editor auto-routes. Ctrl/Cmd+Enter runs.</p>
</div>
<span class="cz-studio-actions-spacer"></span>
<label class="cz-studio-tenant-select" title="Multi-tenant + multi-region routing lands in v1.0. v0.0.x runs against a single tenant on the local cluster.">
<span class="cz-studio-tenant-eyebrow">Tenant</span>
<select disabled><option>default</option></select>
</label>
</div>
{tabs_html}
{flash_html}
<form method="post" action="/studio/sql/execute" id="cz-sql-form">
{csrf}
<input type="hidden" name="open" value="{open_csv}" />
<input type="hidden" name="active" value="{active_id}" />
<div class="cz-studio-editor-toolbar">
<span id="cz-sql-lang-pill" class="cz-studio-engine-pill" data-lang="{lang_attr}">{lang_label}</span>
<span class="cz-studio-actions-spacer"></span>
{file_actions_html}
</div>
<div class="cz-studio-editor-wrap">
<textarea id="cz-sql-textarea" name="sql" class="cz-studio-editor-textarea" placeholder="{sql_placeholder}">{sql_value}</textarea>
<div id="cz-sql-monaco" data-initial="{sql_value_attr}" style="display: none; height: 18rem;"></div>
</div>
<div class="cz-studio-actions">
{save_button_html}
<button type="submit" class="cz-btn-primary">{sql_run}</button>
<span class="cz-studio-editor-hint" style="margin-left: 0.25rem;">Ctrl/Cmd+Enter to run.</span>
<div class="cz-studio-actions-spacer"></div>
</div>
<p class="cz-studio-editor-hint">{sql_help}</p>
</form>
<div class="cz-studio-results">
{results_html}
</div>
<script>
// Re-detect SQL vs Python on every keystroke and update the pill so
// the operator sees which engine will pick up their query. Same
// heuristic as the Rust-side detect_query_language: first non-blank
// non-comment line; if it starts with a SQL reserved word it's SQL,
// otherwise Python.
(function() {{
  var pill = document.getElementById("cz-sql-lang-pill");
  var ta = document.getElementById("cz-sql-textarea");
  if (!pill || !ta) return;
  var SQL_HEADS = [
    "SELECT","WITH","INSERT","UPDATE","DELETE","MERGE","CREATE","DROP",
    "ALTER","TRUNCATE","USE","SHOW","DESCRIBE","DESC","EXPLAIN","GRANT",
    "REVOKE","CALL","REFRESH","OPTIMIZE","VACUUM","ANALYZE","COPY","SET"
  ];
  function detect(src) {{
    var lines = (src || "").split(/\r?\n/);
    for (var i = 0; i < lines.length; i++) {{
      var t = lines[i].trim();
      if (!t) continue;
      if (t.indexOf("--") === 0) continue;
      if (t.indexOf("#") === 0) return "python";
      var first = (t.split(/\s+/)[0] || "").toUpperCase().replace(/[^A-Z]/g, "");
      return SQL_HEADS.indexOf(first) >= 0 ? "sql" : "python";
    }}
    return "sql";
  }}
  function paint() {{
    var lang = detect(ta.value);
    pill.dataset.lang = lang;
    pill.textContent = lang === "sql" ? "SQL → Trino" : "Python → Sail";
  }}
  ta.addEventListener("input", paint);
  paint();
}})();
</script>"##,
        csrf = csrf,
        sql_placeholder = html_escape(&sql_placeholder),
        sql_value = html_escape(sql_prefill),
        sql_value_attr = html_escape(sql_prefill),
        sql_run = html_escape(&sql_run),
        sql_help = html_escape(&sql_help),
        results_html = results_html,
        lang_attr = match initial_language { QueryLanguage::Sql => "sql", QueryLanguage::Python => "python" },
        lang_label = initial_lang_label,
        tabs_html = tabs_html,
        flash_html = flash_html,
        open_csv = html_escape(&open_csv),
        active_id = html_escape(&active_id),
        save_button_html = save_button_html,
        file_actions_html = file_actions_html,
    );

    // ---- Files pane (middle column) ---------------------------------
    let files_pane_html = render_studio_files_pane(files);

    let body = format!(
        r#"<div class="cz-studio-shell">
<aside class="cz-studio-sidebar">{sidebar_html}</aside>
<aside class="cz-studio-files">{files_pane_html}</aside>
<main class="cz-studio-main">{main_html}</main>
</div>
<script>
// Monaco editor progressive enhancement.
//
// Loads Monaco 0.45.0 from a CDN (pinned -- "latest" would expose
// us to upstream breaking changes). On success: hides the textarea,
// mounts Monaco in its place, copies Monaco's value back into the
// textarea on submit so the form POST carries the right body. On
// failure (CDN blocked, JS disabled, network down) the textarea
// stays visible and the form works unchanged -- no functionality
// loss, just no syntax highlighting.
//
// Ctrl/Cmd+Enter binds to form submit, matching the convention in
// every other SQL IDE.
(function() {{
  var mount = document.getElementById("cz-sql-monaco");
  var textarea = document.getElementById("cz-sql-textarea");
  var form = document.getElementById("cz-sql-form");
  if (!mount || !textarea || !form) return;

  // Pin the Monaco version explicitly. Auto-latest would silently
  // change behaviour; a pinned version is what every other CDN dep
  // in this repo does (see fetch.rs Bundle pins for the same
  // reasoning applied to component binaries).
  var MONACO_VERSION = "0.45.0";
  var BASE = "https://cdn.jsdelivr.net/npm/monaco-editor@" + MONACO_VERSION + "/min";

  // Monaco's loader.js bootstraps an AMD `require` and pulls the
  // editor bundle. Inject loader.js, then call require() for the
  // editor.main module. If loader.js itself fails to load (network
  // blocked, etc) the onerror keeps the textarea visible and we
  // exit silently.
  var script = document.createElement("script");
  script.src = BASE + "/vs/loader.js";
  script.onerror = function() {{
    // CDN unreachable. Textarea stays visible; nothing to do.
    console.warn("computeza studio: Monaco CDN unreachable; falling back to textarea editor");
  }};
  script.onload = function() {{
    if (typeof require !== "function" || !require.config) return;
    require.config({{ paths: {{ vs: BASE + "/vs" }} }});
    require(["vs/editor/editor.main"], function() {{
      var initial = mount.getAttribute("data-initial") || "";
      // Decode the HTML-encoded initial value back to plain text
      // for Monaco. The server encoded &lt; &gt; &amp; &quot; &#39;
      // and we reverse those four below; if other entities show up
      // we'll add them when they do.
      initial = initial
        .replace(/&amp;/g, "&")
        .replace(/&lt;/g, "<")
        .replace(/&gt;/g, ">")
        .replace(/&quot;/g, '"')
        .replace(/&#39;/g, "'");

      var editor = monaco.editor.create(mount, {{
        value: initial,
        language: "sql",
        theme: "vs-dark",
        automaticLayout: true,
        minimap: {{ enabled: false }},
        scrollBeyondLastLine: false,
        wordWrap: "on",
        fontSize: 13,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        // Trigger characters: most SQL completion lands on space
        // or dot (qualifying a table). Keep the list small so we
        // don't fight Monaco's built-in keyword completion which
        // already triggers on letters.
        suggest: {{ showWords: true }},
      }});

      // Schema-aware completion provider. Fetches from
      // /studio/api/completions which aggregates Databend
      // databases + tables, Lakekeeper warehouses, and recent
      // history. Cached for 30s in-process so each keystroke
      // doesn't re-hit the server.
      var completionCache = {{ items: null, expires: 0 }};
      function fetchCompletions() {{
        var now = Date.now();
        if (completionCache.items && completionCache.expires > now) {{
          return Promise.resolve(completionCache.items);
        }}
        return fetch("/studio/api/completions", {{ credentials: "same-origin" }})
          .then(function(r) {{ return r.ok ? r.json() : {{ items: [] }}; }})
          .then(function(j) {{
            completionCache = {{ items: j.items || [], expires: Date.now() + 30000 }};
            return completionCache.items;
          }})
          .catch(function() {{ return []; }});
      }}
      // Pre-warm so the first Ctrl+Space hits a populated cache.
      fetchCompletions();

      var kindMap = {{
        database:  monaco.languages.CompletionItemKind.Folder,
        table:     monaco.languages.CompletionItemKind.Class,
        column:    monaco.languages.CompletionItemKind.Field,
        warehouse: monaco.languages.CompletionItemKind.Module,
        history:   monaco.languages.CompletionItemKind.Snippet,
      }};

      monaco.languages.registerCompletionItemProvider("sql", {{
        triggerCharacters: [" ", "."],
        provideCompletionItems: function(model, position) {{
          return fetchCompletions().then(function(items) {{
            var word = model.getWordUntilPosition(position);
            var range = {{
              startLineNumber: position.lineNumber,
              endLineNumber: position.lineNumber,
              startColumn: word.startColumn,
              endColumn: word.endColumn,
            }};
            return {{
              suggestions: items.map(function(it) {{
                return {{
                  label: it.label,
                  kind: kindMap[it.kind] || monaco.languages.CompletionItemKind.Text,
                  insertText: it.insert || it.label,
                  detail: it.detail || "",
                  range: range,
                }};
              }}),
            }};
          }});
        }},
      }});

      // Swap textarea for Monaco. Keep textarea in the DOM so the
      // form submission still finds its `name="sql"` field; we
      // just stop displaying it.
      textarea.style.display = "none";
      mount.style.display = "block";

      // Ctrl/Cmd+Enter submits the form. Standard SQL-IDE shortcut.
      editor.addCommand(
        monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter,
        function() {{ form.requestSubmit(); }}
      );

      // Sync Monaco -> textarea before submit so the POST body
      // carries the current editor contents (not the original
      // server-rendered value).
      form.addEventListener("submit", function() {{
        textarea.value = editor.getValue();
      }});

      // Move focus into the editor on page load so the operator
      // can start typing immediately. Skip if the editor is
      // pre-filled with a SELECT * the operator just clicked
      // (let them inspect first).
      if (!initial.trim()) editor.focus();
    }});
  }};
  document.head.appendChild(script);
}})();
</script>"#,
        sidebar_html = sidebar_html,
        main_html = main_html,
    );

    render_shell(localizer, &title, NavLink::Studio, &body)
}

/// Minimal percent-encoder used to pass the prefill-SQL through the
/// query string. The set of unsafe characters here is conservative
/// (anything not unreserved-per-RFC-3986 gets `%XX`-encoded). We
/// pull this in inline rather than depending on the `urlencoding`
/// crate because the studio surface is the only caller; a
/// general-purpose helper would land in a util crate when the
/// second caller shows up.
///
/// Currently dead-code-warned because the table-prefill links that
/// used it were removed when the catalog pane pivoted to surfacing
/// Lakekeeper warehouse state (commit removing the create-namespace
/// handler). Will be re-introduced in phase 1.5 once warehouse
/// bootstrap ships and Iceberg namespaces drill-down lands; keeping
/// the helper here so the next iteration doesn't redefine it.
#[allow(dead_code)]
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ============================================================
// Lakekeeper bootstrap wizard (creates the project + warehouse +
// storage credentials so the catalog browser stops returning "no
// warehouses configured").
//
// This v0.0.x wizard is operator-driven (the form takes S3
// credentials by hand). v0.1 will auto-mint Garage credentials via
// the Garage admin API + auto-populate this same form. The shape
// here is forward-compatible: the POST handler is the only piece
// that needs to change, the form fields stay the same.
//
// IMPORTANT: the exact Lakekeeper management API field names
// (`project-name` vs `name`, `storage-profile` vs `storage_profile`,
// etc.) drift across Lakekeeper releases. We try a sensible default
// and surface the API response verbatim on failure so the operator
// can adjust the form values (or so we can iterate the field names
// in the driver).
// ============================================================

/// Form body for `POST /studio/bootstrap`.
#[derive(serde::Deserialize)]
struct StudioBootstrapForm {
    project_name: String,
    warehouse_name: String,
    s3_endpoint: String,
    s3_region: String,
    s3_bucket: String,
    s3_access_key: String,
    s3_secret_access_key: String,
}

async fn studio_bootstrap_form_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<FlashQuery>,
) -> Html<String> {
    let l = Localizer::english();
    let lakekeeper = discover_lakekeeper_endpoint(state.store.as_deref()).await;
    let garage = discover_garage_endpoint(state.store.as_deref()).await;
    let suggested_s3 = garage
        .as_ref()
        .map(|u| u.replace(":3903", ":3900"))
        .unwrap_or_else(|| "http://127.0.0.1:3900".to_string());
    let prefill = read_garage_credentials_for_prefill(state.secrets.as_deref()).await;
    // The `flash` query param carries the post-reset summary so the
    // operator sees what happened after the redirect.
    let flash_result = q
        .flash
        .as_deref()
        .map(|s| Ok::<String, String>(s.to_string()));
    Html(render_studio_bootstrap_form(
        &l,
        lakekeeper.is_some(),
        &suggested_s3,
        &prefill,
        flash_result,
    ))
}

/// POST /studio/bootstrap/reset
///
/// Destructive recovery action: drops + recreates the lakekeeper
/// postgres database, restarts the service, and clears the vault's
/// cached warehouse identifiers so the next /studio/bootstrap mints
/// fresh ones. Use when the postgres state has drifted from what
/// Studio expects (orphaned warehouses in non-default projects,
/// duplicate names across projects, ...) and a clean re-bootstrap
/// is the only way to recover without touching the shell.
///
/// The handler runs synchronously rather than spawning a job because
/// the reset is fast (~5s including a 30s readiness ceiling) and the
/// operator is sitting on the form waiting for the next step.
async fn studio_bootstrap_reset_handler(
    State(state): State<AppState>,
) -> axum::response::Redirect {
    #[cfg(target_os = "linux")]
    {
        let result = computeza_driver_native::linux::lakekeeper::reset_state().await;
        // Clear the vault's cached warehouse identifiers so the
        // next bootstrap is a true fresh start. Per-name entries
        // can't be enumerated without a list operation; the
        // singleton key + the well-known sail catalog index cover
        // the common cases.
        if let Some(s) = state.secrets.as_deref() {
            let _ = s.delete("lakekeeper/default-warehouse-id").await;
            let _ = s.delete("lakekeeper/default-warehouse-name").await;
            let _ = s.delete("sail/catalog-names").await;
            // Best-effort: also nuke any per-name keys that the
            // resolver may have written. We can't enumerate, but
            // re-bootstrap will overwrite them.
        }
        let mut detail = String::from("Lakekeeper reset complete:\n");
        for step in &result.steps {
            detail.push_str(&format!("  ✓ {step}\n"));
        }
        for warn in &result.warnings {
            detail.push_str(&format!("  ! {warn}\n"));
        }
        detail.push_str("\nVault warehouse cache cleared.\n\nNext: re-run /studio/bootstrap below to provision a fresh warehouse.");
        axum::response::Redirect::to(&format!(
            "/studio/bootstrap?flash={}",
            url_encode(&detail)
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = state; // suppress unused on non-linux
        axum::response::Redirect::to(&format!(
            "/studio/bootstrap?flash={}",
            url_encode("Reset is Linux-only in v0.0.x.")
        ))
    }
}

async fn studio_bootstrap_submit_handler(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<StudioBootstrapForm>,
) -> Html<String> {
    let l = Localizer::english();
    let prefill = read_garage_credentials_for_prefill(state.secrets.as_deref()).await;
    let Some(lakekeeper_url) = discover_lakekeeper_endpoint(state.store.as_deref()).await else {
        return Html(render_studio_bootstrap_form(
            &l,
            false,
            &form.s3_endpoint,
            &prefill,
            Some(Err(
                "Lakekeeper is not installed; bootstrap cannot proceed. Install Lakekeeper from /install first.".to_string(),
            )),
        ));
    };
    let outcome = run_lakekeeper_bootstrap(&lakekeeper_url, &form).await;
    // On success, persist BOTH the Garage credentials AND the
    // warehouse name + UUID to vault so:
    //   1. subsequent auto-bootstrap re-runs find the same values
    //   2. probe_lakekeeper_with_vault_fallback finds the warehouse
    //      name even if Lakekeeper's warehouse-list returns empty
    //   3. drill-down handlers can resolve `name -> UUID` before
    //      hitting Lakekeeper's /catalog/v1/{uuid}/* endpoints
    //      (which reject names with "WarehouseIdIsNotUUID")
    if let Ok(ok) = &outcome {
        if let Some(secrets) = state.secrets.as_deref() {
            let _ = secrets
                .put("garage/lakekeeper-key-id", &form.s3_access_key)
                .await;
            let _ = secrets
                .put("garage/lakekeeper-secret", &form.s3_secret_access_key)
                .await;
            let _ = secrets
                .put("garage/lakekeeper-bucket", &form.s3_bucket)
                .await;
            let _ = secrets
                .put("lakekeeper/default-warehouse-name", &form.warehouse_name)
                .await;
            if let Some(id) = &ok.warehouse_id {
                let _ = secrets
                    .put("lakekeeper/default-warehouse-id", id)
                    .await;
            }
        }
    }
    // After Lakekeeper succeeds, register this warehouse with both
    // engines:
    //   1. Trino (SQL): drop an Iceberg-REST catalog .properties file
    //      under etc/catalog/ AND fire CREATE CATALOG via Trino's HTTP
    //      `/v1/statement` so the catalog is usable immediately
    //      without restarting the coordinator (catalog-management =
    //      DYNAMIC is set by the driver).
    //   2. Sail (Python / Spark Connect): there's no DDL equivalent
    //      for Spark catalog config -- it's set per-SparkSession at
    //      build time. We persist the catalog config to vault so the
    //      Studio Python execution path can inject it into the
    //      `SparkSession.builder.config(...)` chain on every query.
    // Both are best-effort: a missing engine doesn't fail bootstrap.
    let (trino_wiring, sail_wiring) = if outcome.is_ok() {
        let tw = wire_trino_iceberg_catalog(
            state.store.as_deref(),
            state.secrets.as_deref(),
            &lakekeeper_url,
            &form,
        )
        .await;
        let saw = persist_sail_catalog_config(
            state.store.as_deref(),
            state.secrets.as_deref(),
            &lakekeeper_url,
            &form,
            outcome.as_ref().ok().and_then(|o| o.warehouse_id.clone()),
        )
        .await;
        (Some(tw), Some(saw))
    } else {
        (None, None)
    };
    let display_outcome: Result<String, String> = match outcome {
        Ok(ok) => {
            let trino_line = match trino_wiring {
                Some(TrinoWiringOutcome::Wired { catalog_name }) => format!(
                    "\n - Trino SQL catalog `{catalog_name}` registered. Run \
                     `SELECT * FROM {catalog_name}.<schema>.<table>` in the \
                     SQL editor (queries auto-route to Trino)."
                ),
                Some(TrinoWiringOutcome::TrinoUnavailable) => "\n - Trino SQL wiring skipped: Trino isn't installed. Install it from /install + re-run bootstrap to enable SQL queries.".to_string(),
                Some(TrinoWiringOutcome::Failed { reason }) => format!(
                    "\n - Trino SQL wiring FAILED: {reason}\n   Re-run bootstrap or use the \"Connect to SQL\" button on the warehouse page."
                ),
                None => String::new(),
            };
            let sail_line = match sail_wiring {
                Some(SailCatalogOutcome::Configured { catalog_name }) => format!(
                    "\n - Sail Spark catalog `{catalog_name}` configured. Write PySpark / DataFrame code \
                     in the editor (queries auto-route to Sail)."
                ),
                Some(SailCatalogOutcome::SailUnavailable) => "\n - Sail Python wiring skipped: Sail isn't installed. Install it from /install + re-run bootstrap to enable Python/Spark queries.".to_string(),
                Some(SailCatalogOutcome::ConfigPersistFailed { reason }) => format!(
                    "\n - Sail Spark catalog config FAILED to persist: {reason}\n   Vault is degraded; this needs operator attention."
                ),
                None => String::new(),
            };
            Ok(format!("{}{}{}", ok.message, trino_line, sail_line))
        }
        Err(e) => Err(e),
    };
    Html(render_studio_bootstrap_form(
        &l,
        true,
        &form.s3_endpoint,
        &prefill,
        Some(display_outcome),
    ))
}

/// Pre-fill helper: read the three Garage credential entries from
/// the vault if they're there. Used by /studio/bootstrap to
/// pre-populate the form fields so the operator doesn't have to
/// re-type them when running this form as an escape hatch (e.g.
/// after a partial auto-bootstrap failure, or to test changes).
#[derive(Debug, Default, Clone)]
struct GarageCredentialPrefill {
    key_id: String,
    secret: String,
    bucket: String,
}

async fn read_garage_credentials_for_prefill(
    secrets: Option<&computeza_secrets::SecretsStore>,
) -> GarageCredentialPrefill {
    use secrecy::ExposeSecret;
    let mut out = GarageCredentialPrefill::default();
    let Some(s) = secrets else { return out };
    if let Ok(Some(v)) = s.get("garage/lakekeeper-key-id").await {
        out.key_id = v.expose_secret().to_string();
    }
    if let Ok(Some(v)) = s.get("garage/lakekeeper-secret").await {
        out.secret = v.expose_secret().to_string();
    }
    if let Ok(Some(v)) = s.get("garage/lakekeeper-bucket").await {
        out.bucket = v.expose_secret().to_string();
    }
    out
}

/// Same shape as the lakekeeper / databend discovery helpers, for
/// Garage. The bootstrap form pre-fills the S3 endpoint from this.
async fn discover_garage_endpoint(
    store: Option<&computeza_state::SqliteStore>,
) -> Option<String> {
    use computeza_state::Store;
    let store = store?;
    let rows = store.list("garage-instance", None).await.ok()?;
    let sr = rows.into_iter().next()?;
    let spec: StudioEndpointSpec = serde_json::from_value(sr.spec).ok()?;
    Some(spec.endpoint.base_url)
}

/// Run the two-step bootstrap against a live Lakekeeper. Returns
/// either a success message (with the warehouse name confirmed) or
/// a detailed error string the operator can use to adjust the
/// form. Best-effort: never panics, surfaces HTTP body verbatim on
/// non-2xx.
/// Result type for `run_lakekeeper_bootstrap`: a success message
/// for inline display plus the warehouse UUID we captured (or
/// looked up) for the just-created warehouse. The UUID is what
/// Lakekeeper's Iceberg REST `/catalog/v1/{warehouse-id}/*`
/// endpoints expect as the URL prefix; the human-readable name is
/// only a display label on the management side.
struct LakekeeperBootstrapOk {
    message: String,
    warehouse_id: Option<String>,
}

/// Escape a string for use inside a Databend SQL single-quoted
/// literal. Databend follows SQL standard: a literal `'` is escaped
/// as `''`. Backslashes do NOT need escaping in standard-mode quotes.
/// This is the only sanitization the bootstrap wiring needs because
/// the catalog name itself comes from sanitize_sql_identifier (alpha-
/// numerics + `_` only), and all other interpolated values are
/// wrapped in single-quoted literals.
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Sanitize a warehouse name into a SQL identifier acceptable to
/// Databend's `CREATE CATALOG <name>` syntax. Databend identifiers
/// must start with a letter or underscore and contain only ASCII
/// alphanumerics + `_`. Anything else collapses to `_`. If the input
/// starts with a digit we prefix `cat_` so the identifier is valid.
fn sanitize_sql_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "warehouse".to_string();
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return format!("cat_{out}");
    }
    out
}

/// Outcome of persisting the Sail Spark-catalog config for a
/// newly-bootstrapped warehouse. Unlike Databend, Sail doesn't take
/// CREATE CATALOG DDL -- catalog config is per-SparkSession at build
/// time. We persist the config in vault and the Studio Python
/// executor injects it into every SparkSession.builder it creates.
#[derive(Debug)]
enum SailCatalogOutcome {
    /// Sail isn't installed -- nothing to record.
    SailUnavailable,
    /// Catalog config persisted to vault. Studio Python query path
    /// will inject these conf entries into every SparkSession.
    Configured { catalog_name: String },
    /// Vault write failed -- the operator should investigate.
    ConfigPersistFailed { reason: String },
}

/// Persist the per-warehouse Spark/Iceberg catalog config to vault
/// so the Studio Python query path can inject it into
/// `SparkSession.builder.config(...)` for every Sail query. Vault
/// keys are namespaced `sail/catalog/<catalog-name>/...` so future
/// multi-warehouse support drops in without a schema change.
async fn persist_sail_catalog_config(
    store: Option<&computeza_state::SqliteStore>,
    secrets: Option<&computeza_secrets::SecretsStore>,
    lakekeeper_url: &str,
    form: &StudioBootstrapForm,
    warehouse_uuid: Option<String>,
) -> SailCatalogOutcome {
    // Sail-not-installed shortcut: no point persisting config if
    // there's no engine to consume it.
    if discover_sail_endpoint(store).await.is_none() {
        return SailCatalogOutcome::SailUnavailable;
    }
    let Some(secrets) = secrets else {
        return SailCatalogOutcome::ConfigPersistFailed {
            reason: "Vault is not configured; cannot persist Sail catalog config.".into(),
        };
    };
    let catalog_name = sanitize_sql_identifier(&form.warehouse_name);
    // Prefer the caller-supplied UUID (bootstrap captured it from
    // Lakekeeper's create-response). Otherwise use the vault-first
    // resolver so we get the UUID drill-down auto-recovery has
    // validated -- matches what Databend wire path uses.
    let uuid = if let Some(id) = warehouse_uuid {
        id
    } else {
        resolve_warehouse_id_or_pass(&form.warehouse_name, Some(lakekeeper_url), Some(secrets))
            .await
    };
    // Persist as individual keys so a future "rotate one field" can
    // touch a single vault entry. Lakekeeper-REST URL stays as a
    // discoverable endpoint (not persisted) -- the executor reads
    // it via discover_lakekeeper_endpoint at query time.
    let kv = [
        (
            format!("sail/catalog/{catalog_name}/warehouse-uuid"),
            uuid.as_str(),
        ),
        (
            format!("sail/catalog/{catalog_name}/s3-endpoint"),
            form.s3_endpoint.as_str(),
        ),
        (
            format!("sail/catalog/{catalog_name}/s3-region"),
            form.s3_region.as_str(),
        ),
        (
            format!("sail/catalog/{catalog_name}/s3-access-key-id"),
            form.s3_access_key.as_str(),
        ),
        (
            format!("sail/catalog/{catalog_name}/s3-secret-access-key"),
            form.s3_secret_access_key.as_str(),
        ),
    ];
    for (k, v) in &kv {
        if let Err(e) = secrets.put(k, v).await {
            return SailCatalogOutcome::ConfigPersistFailed {
                reason: format!("vault put {k}: {e}"),
            };
        }
    }
    // Also persist the canonical catalog-name list so the executor
    // can enumerate which Sail catalogs to register on a session.
    // Append-only set: read current, add if missing, write back.
    let key = "sail/catalog-names";
    let existing = secrets
        .get(key)
        .await
        .ok()
        .flatten()
        .map(|v| {
            use secrecy::ExposeSecret;
            v.expose_secret().to_string()
        })
        .unwrap_or_default();
    let mut names: Vec<&str> = existing.split(',').filter(|s| !s.is_empty()).collect();
    if !names.iter().any(|n| *n == catalog_name) {
        names.push(&catalog_name);
        let joined = names.join(",");
        if let Err(e) = secrets.put(key, &joined).await {
            return SailCatalogOutcome::ConfigPersistFailed {
                reason: format!("vault put {key}: {e}"),
            };
        }
    }
    SailCatalogOutcome::Configured { catalog_name }
}

/// Outcome of attempting to wire Trino up to the just-bootstrapped
/// Lakekeeper warehouse so `SELECT * FROM <catalog>.<namespace>.<tbl>`
/// resolves in the SQL editor.
#[derive(Debug)]
enum TrinoWiringOutcome {
    /// Trino isn't installed / discoverable -- nothing to wire.
    /// Bootstrap still succeeds; the operator just has to install
    /// Trino later and re-run bootstrap to enable SQL access.
    TrinoUnavailable,
    /// The `etc/catalog/<name>.properties` file landed and CREATE
    /// CATALOG executed against the running coordinator. The catalog
    /// is usable in the editor immediately and survives a coordinator
    /// restart (the .properties file is read on startup).
    Wired { catalog_name: String },
    /// Trino accepted neither the dynamic CREATE CATALOG nor was the
    /// catalog already present. Carries the raw error so the operator
    /// can drill into Trino's response.
    Failed { reason: String },
}

/// Auto-wire Trino → Lakekeeper-Iceberg in two steps:
///   1. Write `etc/catalog/<name>.properties` via the Trino driver
///      helper. This is the file Trino's static catalog loader picks
///      up on startup -- so the catalog survives a coordinator
///      restart.
///   2. Submit `DROP CATALOG IF EXISTS` + `CREATE CATALOG <name>
///      USING iceberg WITH (...)` against the running coordinator
///      via `/v1/statement`. Trino's `catalog-management=DYNAMIC`
///      flag (set in our generated config.properties) makes the
///      catalog immediately usable from the editor without a restart.
///
/// Best-effort: a Trino outage or an Iceberg-REST mismatch surfaces
/// `Failed { reason }`, but the bootstrap step itself still succeeds.
async fn wire_trino_iceberg_catalog(
    store: Option<&computeza_state::SqliteStore>,
    secrets: Option<&computeza_secrets::SecretsStore>,
    lakekeeper_url: &str,
    form: &StudioBootstrapForm,
) -> TrinoWiringOutcome {
    let Some(trino_url) = discover_trino_endpoint(store).await else {
        return TrinoWiringOutcome::TrinoUnavailable;
    };
    let catalog_name = sanitize_sql_identifier(&form.warehouse_name);
    let rest_address = format!("{}/catalog", lakekeeper_url.trim_end_matches('/'));

    // Per Lakekeeper's official Trino example, the `warehouse` config
    // option holds the human warehouse NAME -- the Iceberg-REST
    // client resolves NAME -> UUID-prefix on its own via /v1/config.
    // For belt-and-braces, we also try the vault-cached UUID + the
    // /v1/config-probed UUID if the name fails.
    let mut candidates: Vec<String> = Vec::with_capacity(3);
    candidates.push(form.warehouse_name.clone());
    let vault_uuid =
        resolve_warehouse_id_or_pass(&form.warehouse_name, Some(lakekeeper_url), secrets).await;
    if !candidates.iter().any(|c| c == &vault_uuid) {
        candidates.push(vault_uuid);
    }
    if let Some(probed) =
        try_discover_via_config(&form.warehouse_name, lakekeeper_url, secrets).await
    {
        if !candidates.iter().any(|c| c == &probed) {
            candidates.push(probed);
        }
    }

    // Drop the existing catalog once before trying candidates so
    // re-runs don't trip over a stale binding.
    let drop_sql = format!("DROP CATALOG IF EXISTS {catalog_name}");
    if let SqlOutcome::Err(e) = execute_sql_against_trino(&trino_url, &drop_sql).await {
        tracing::info!(
            catalog = %catalog_name,
            error = %e,
            "wire_trino: DROP CATALOG IF EXISTS returned err (treating as ignorable)"
        );
    }

    let mut attempts: Vec<(String, String)> = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        // Step 1: write the .properties file so the catalog survives
        // a coordinator restart. The driver helper resolves the
        // install root via /var/lib/computeza/trino/binaries/*/.
        // Trino itself only installs on Linux right now -- on other
        // platforms the discover step would have already returned
        // None, but we gate the call anyway so the binary compiles
        // cross-platform.
        #[cfg(target_os = "linux")]
        {
            let cfg = computeza_driver_native::linux::trino::TrinoIcebergRestConfig {
                rest_catalog_uri: rest_address.clone(),
                warehouse: candidate.clone(),
                s3_endpoint: form.s3_endpoint.clone(),
                s3_region: form.s3_region.clone(),
                s3_access_key: form.s3_access_key.clone(),
                s3_secret_key: form.s3_secret_access_key.clone(),
            };
            if let Err(e) =
                computeza_driver_native::linux::trino::write_iceberg_rest_catalog_file(
                    std::path::Path::new("/var/lib/computeza/trino"),
                    &catalog_name,
                    &cfg,
                )
                .await
            {
                tracing::warn!(
                    catalog = %catalog_name,
                    error = %e,
                    "wire_trino: failed to write etc/catalog/{catalog_name}.properties (catalog won't survive coordinator restart, but CREATE CATALOG may still succeed)"
                );
            }
        }

        // Step 2: CREATE CATALOG so the catalog is immediately usable
        // in the editor (catalog-management=DYNAMIC is on).
        //
        // Trino's CREATE CATALOG WITH-clause keys are the Iceberg
        // connector's full property names -- same shape as the
        // .properties file. Lowercase, double-quoted because they
        // contain dots; values are SQL single-quoted strings.
        let create_sql = format!(
            "CREATE CATALOG {name} USING iceberg WITH (\
                \"iceberg.catalog.type\" = 'rest', \
                \"iceberg.rest-catalog.uri\" = '{uri}', \
                \"iceberg.rest-catalog.warehouse\" = '{warehouse}', \
                \"fs.s3.enabled\" = 'true', \
                \"s3.endpoint\" = '{s3_endpoint}', \
                \"s3.region\" = '{s3_region}', \
                \"s3.path-style-access\" = 'true', \
                \"s3.aws-access-key\" = '{ak}', \
                \"s3.aws-secret-key\" = '{sk}'\
            )",
            name = catalog_name,
            uri = sql_quote(&rest_address),
            warehouse = sql_quote(candidate),
            s3_endpoint = sql_quote(&form.s3_endpoint),
            s3_region = sql_quote(&form.s3_region),
            ak = sql_quote(&form.s3_access_key),
            sk = sql_quote(&form.s3_secret_access_key),
        );
        tracing::info!(
            catalog = %catalog_name,
            warehouse = %candidate,
            "wire_trino: trying CREATE CATALOG with this warehouse candidate"
        );
        match execute_sql_against_trino(&trino_url, &create_sql).await {
            SqlOutcome::Ok { .. } => {
                tracing::info!(
                    catalog = %catalog_name,
                    warehouse = %candidate,
                    "wire_trino: CREATE CATALOG succeeded -- caching this prefix to vault"
                );
                if let Some(s) = secrets {
                    let _ = s
                        .put("lakekeeper/default-warehouse-id", candidate)
                        .await;
                }
                return TrinoWiringOutcome::Wired { catalog_name };
            }
            SqlOutcome::Err(e) => {
                tracing::warn!(
                    catalog = %catalog_name,
                    warehouse = %candidate,
                    error = %e,
                    "wire_trino: CREATE CATALOG failed for this candidate; trying next"
                );
                attempts.push((candidate.clone(), e));
                // Re-DROP between attempts so leftover state from a
                // partial CREATE doesn't poison the next attempt.
                let _ = execute_sql_against_trino(&trino_url, &drop_sql).await;
            }
        }
    }
    // All candidates failed. Build a verbose error.
    let mut reason = String::from(
        "Trino rejected CREATE CATALOG for every candidate warehouse value we tried. ",
    );
    reason.push_str(&format!("Tried {} candidates:\n", attempts.len()));
    for (cand, err) in &attempts {
        reason.push_str(&format!(
            "\n--- warehouse = '{cand}' ---\n{}\n",
            err.chars().take(800).collect::<String>()
        ));
    }
    reason.push_str(
        "\n--- next steps ---\n\
         1. Reset Lakekeeper: stop computeza-lakekeeper, drop+recreate the \
         lakekeeper Postgres database, restart, re-run /studio/bootstrap.\n\
         2. If that fixes it, the original warehouse rows were corrupt.\n\
         3. If it doesn't, inspect /var/lib/computeza/trino/binaries/*/trino-server-*/var/log/server.log \
         for the Iceberg connector's request to Lakekeeper.",
    );
    TrinoWiringOutcome::Failed { reason }
}

/// Sanitize a warehouse name into an S3 key prefix.
///
/// S3 object keys can contain almost any UTF-8 byte, but Iceberg
/// metadata paths get baked into manifest files and into Lakekeeper's
/// own DB rows, so we keep the prefix conservative: lower-case
/// alphanumerics, dashes, and underscores; everything else collapses
/// to a single `-`. An empty input falls back to `warehouse` so the
/// prefix is never the literal empty string (which would put metadata
/// at the bucket root and collide with other warehouses).
fn sanitize_s3_prefix(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if ch == '-' || ch == '_' {
            out.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('-');
            last_was_sep = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "warehouse".to_string()
    } else {
        trimmed
    }
}

async fn run_lakekeeper_bootstrap(
    base_url: &str,
    form: &StudioBootstrapForm,
) -> Result<LakekeeperBootstrapOk, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))?;

    // --- Step 1: project resolution ----------------------------------
    //
    // CRITICAL DESIGN CHOICE: warehouses go into the nil/default
    // project (UUID = all zeros). Lakekeeper's Iceberg-REST endpoint
    // resolves `/catalog/v1/<warehouse-id>/...` only against the
    // default project when the client doesn't send a project-id
    // header. Databend's Iceberg-REST client doesn't pass one, so
    // a warehouse stored under a custom project is invisible to it
    // (NoSuchWarehouseException on every lookup).
    //
    // Two-step bootstrap to make this work:
    //   a) POST /management/v1/bootstrap -- initializes the default
    //      project (UUID = nil). One-time per Lakekeeper instance;
    //      returns 200 first time, 4xx every subsequent call.
    //   b) POST /management/v1/warehouse with project-id = nil.
    //
    // Without step (a), step (b) returns ProjectNotFound for the
    // nil UUID -- which is exactly the bug we were hitting before.
    // Same nil-project UUID the Iceberg-REST read paths send as
    // x-project-id (see LAKEKEEPER_NIL_PROJECT). Kept in sync.
    const NIL_PROJECT_ID: &str = LAKEKEEPER_NIL_PROJECT;
    let bootstrap_url = format!("{}/management/v1/bootstrap", base_url.trim_end_matches('/'));
    // Body shape from Lakekeeper docs: accept_terms_of_use is
    // required; is_operator marks this caller as the system
    // operator (us). user-* fields are optional; we omit them and
    // let Lakekeeper synthesise a service-principal entry.
    let bootstrap_body = serde_json::json!({
        "accept-terms-of-use": true,
        "is-operator": true,
    });
    let bs_resp = client
        .post(&bootstrap_url)
        .json(&bootstrap_body)
        .send()
        .await
        .map_err(|e| format!("POST {bootstrap_url}: {e}"))?;
    let bs_status = bs_resp.status();
    let bs_text = bs_resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(could not read body: {e})"));
    // Lakekeeper returns 2xx on first bootstrap; 4xx (typically
    // 409 conflict, or 400 "already bootstrapped") on subsequent
    // calls. Either is fine -- we just need the default project
    // to exist by the time we POST /warehouse.
    let bootstrap_was_fresh = bs_status.is_success();
    if !bootstrap_was_fresh
        && !(400..500).contains(&bs_status.as_u16())
        && bs_status.as_u16() != 409
    {
        return Err(format!(
            "POST {bootstrap_url} returned unexpected {} (expected 2xx or 4xx already-bootstrapped):\n{bs_text}",
            bs_status.as_u16()
        ));
    }
    tracing::info!(
        status = bs_status.as_u16(),
        fresh = bootstrap_was_fresh,
        "lakekeeper bootstrap-server endpoint called"
    );
    let project_id = NIL_PROJECT_ID.to_string();
    let project_existed = !bootstrap_was_fresh;
    let project_status = bs_status;
    let _ = &form.project_name; // form field is preserved for display

    // --- Step 2: create warehouse with S3 storage profile +
    // credentials pointing at Garage.
    //
    // Lakekeeper's storage-profile schema for S3 requires several
    // fields that serde refuses to default:
    //   - sts-enabled (bool): we use static access keys, not STS,
    //     so this is always `false`. Required by Lakekeeper's
    //     S3StorageProfile struct.
    //   - flavor (string): "aws" vs "s3-compat". Garage is
    //     S3-compatible but not AWS, so we pick "s3-compat".
    //     Lakekeeper uses this to decide whether to add AWS-only
    //     headers, attempt SigV4-A regional rewrites, etc.
    //   - key-prefix: MUST be unique per warehouse when warehouses
    //     share an S3 bucket. An empty prefix puts metadata at the
    //     bucket root, so a second warehouse using the same bucket
    //     would overwrite the first one's table metadata. We derive
    //     the prefix from the warehouse name (sanitized to s3-safe
    //     chars), which is unique within a project. If the operator
    //     bootstraps a second warehouse with a different bucket they
    //     still get an isolated prefix -- no downside to always
    //     setting one.
    let warehouse_url = format!("{}/management/v1/warehouse", base_url.trim_end_matches('/'));
    let key_prefix = sanitize_s3_prefix(&form.warehouse_name);
    let warehouse_body = serde_json::json!({
        "warehouse-name": form.warehouse_name,
        // Lakekeeper resolves projects by UUID, not name. Sending
        // `project-name` here makes Lakekeeper default project-id
        // to the nil UUID and reject with ProjectNotFound. The
        // project-id was captured above from the create-or-list
        // response.
        "project-id": project_id,
        "storage-profile": {
            "type": "s3",
            "bucket": form.s3_bucket,
            "endpoint": form.s3_endpoint,
            "region": form.s3_region,
            "path-style-access": true,
            "sts-enabled": false,
            "flavor": "s3-compat",
            "key-prefix": key_prefix,
        },
        "storage-credential": {
            "type": "s3",
            "credential-type": "access-key",
            "aws-access-key-id": form.s3_access_key,
            "aws-secret-access-key": form.s3_secret_access_key,
        },
    });
    // x-project-id header MUST be present. Lakekeeper resolves the
    // warehouse's owning project from the header context; the
    // `project-id` field in the body is ignored when auth is
    // disabled. Without this header, the warehouse goes into an
    // implicit project that's NOT the nil default, so subsequent
    // /catalog/v1/<warehouse-id>/... lookups (Iceberg-REST default
    // project context) return NoSuchWarehouseException -- exactly
    // the symptom we've been chasing.
    let warehouse_resp = client
        .post(&warehouse_url)
        .header("x-project-id", &project_id)
        .json(&warehouse_body)
        .send()
        .await
        .map_err(|e| format!("POST {warehouse_url}: {e}"))?;
    let warehouse_status = warehouse_resp.status();
    let warehouse_text = warehouse_resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(could not read body: {e})"));
    // 409 Conflict OR a body that mentions "already exists" means the
    // warehouse with this name was created by a previous run. Treat
    // as success so auto-bootstrap is idempotent.
    let warehouse_existed = warehouse_status.as_u16() == 409
        || warehouse_text.contains("already exists")
        || warehouse_text.contains("WarehouseAlreadyExists");
    if !warehouse_status.is_success() && !warehouse_existed {
        return Err(format!(
            "POST {warehouse_url} returned {}:\n{warehouse_text}\n\n\
             (Project step: {}.) Adjust storage-profile / \
             storage-credential field names in \
             run_lakekeeper_bootstrap() if Lakekeeper expects a \
             different shape for this release.",
            warehouse_status.as_u16(),
            if project_existed {
                "project already existed; reused".to_string()
            } else {
                format!("project created (HTTP {})", project_status.as_u16())
            }
        ));
    }

    // Capture the warehouse UUID. Lakekeeper's create-warehouse
    // response shape includes the UUID on the 201/200 path; on a
    // 409 (warehouse already existed) we need to look it up via
    // the management API. Tolerant of field-name drift:
    // `warehouse-id` / `warehouseId` / `id`.
    let parse_warehouse_id = |body: &str| -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        v.get("warehouse-id")
            .or_else(|| v.get("warehouseId"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    let warehouse_id: Option<String> = if !warehouse_existed {
        parse_warehouse_id(&warehouse_text)
    } else {
        // 409 path: look up the existing warehouse by name. List
        // warehouses scoped to the project we just created/reused.
        let list_url = format!(
            "{}/management/v1/warehouse?project-id={}",
            base_url.trim_end_matches('/'),
            project_id
        );
        let id_from_list = match client
            .get(&list_url)
            .header("x-project-id", &project_id)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.text().await.ok(),
            _ => None,
        };
        id_from_list.and_then(|text| {
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            let arr = v
                .get("warehouses")
                .or_else(|| v.get("data"))
                .and_then(|x| x.as_array())
                .cloned()
                .or_else(|| v.as_array().cloned())
                .unwrap_or_default();
            arr.iter()
                .find(|w| {
                    w.get("name")
                        .or_else(|| w.get("warehouse-name"))
                        .or_else(|| w.get("warehouseName"))
                        .and_then(|n| n.as_str())
                        == Some(form.warehouse_name.as_str())
                })
                .and_then(|w| {
                    w.get("warehouse-id")
                        .or_else(|| w.get("warehouseId"))
                        .or_else(|| w.get("id"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                })
        })
    };

    Ok(LakekeeperBootstrapOk {
        message: format!(
            "Lakekeeper bootstrap succeeded:\n\
             - Project: `{}` ({})\n\
             - Warehouse: `{}` -> S3 bucket `{}` at {}\n\
             - Warehouse UUID: {}\n\
             \n\
             The /studio catalog pane will now show this warehouse with a clickable drill-down.",
            form.project_name,
            if project_existed { "already existed; reused" } else { "created" },
            form.warehouse_name,
            form.s3_bucket,
            form.s3_endpoint,
            warehouse_id.as_deref().unwrap_or("(could not capture; iceberg-REST drill-down may not work)"),
        ),
        warehouse_id,
    })
}

fn render_studio_bootstrap_form(
    localizer: &Localizer,
    has_lakekeeper: bool,
    suggested_s3_endpoint: &str,
    prefill: &GarageCredentialPrefill,
    result: Option<Result<String, String>>,
) -> String {
    let title = localizer.t("ui-studio-bootstrap-title");
    let intro = localizer.t("ui-studio-bootstrap-intro");
    let no_lakekeeper = localizer.t("ui-studio-error-no-lakekeeper");
    let csrf = auth::csrf_input();

    if !has_lakekeeper {
        let body = format!(
            r#"<div class="cz-setup-shell">
<p class="cz-setup-eyebrow">Catalog setup</p>
<h1 class="cz-setup-title">{title}</h1>
<div class="cz-setup-card">
<p style="margin: 0; color: var(--muted); line-height: 1.6;">{message}</p>
<div class="cz-setup-actions">
<a href="/studio" class="cz-btn-ghost">Back to Studio</a>
</div>
</div>
</div>"#,
            title = html_escape(&title),
            message = html_escape(&no_lakekeeper),
        );
        return render_shell(localizer, &title, NavLink::Studio, &body);
    }

    let result_block = match result {
        None => String::new(),
        Some(Ok(msg)) => format!(
            r#"<div class="cz-setup-result-ok">
<p class="cz-setup-result-title">Bootstrap succeeded</p>
<pre class="cz-setup-result-body">{}</pre>
</div>"#,
            html_escape(&msg)
        ),
        Some(Err(msg)) => format!(
            r#"<div class="cz-setup-result-err">
<p class="cz-setup-result-title">Bootstrap failed</p>
<pre class="cz-setup-result-body">{}</pre>
</div>"#,
            html_escape(&msg)
        ),
    };

    let prefill_bucket = if prefill.bucket.is_empty() {
        "lakekeeper-default"
    } else {
        &prefill.bucket
    };

    let body = format!(
        r#"<div class="cz-setup-shell">
<p class="cz-setup-eyebrow">Catalog setup</p>
<h1 class="cz-setup-title">{title}</h1>
<p class="cz-setup-subtitle">{intro}</p>
{result_block}
<form method="post" action="/studio/bootstrap" class="cz-setup-card">
{csrf}
<div class="cz-setup-section">
<p class="cz-setup-section-title">Catalog identity</p>
<p class="cz-setup-section-hint">Name the Lakekeeper project + warehouse that Studio will browse. Defaults are fine for single-tenant installs.</p>
<div class="cz-setup-field">
<label for="bs-project">Project name</label>
<input id="bs-project" name="project_name" class="cz-input" type="text" value="computeza-default" required />
</div>
<div class="cz-setup-field">
<label for="bs-warehouse">Warehouse name</label>
<input id="bs-warehouse" name="warehouse_name" class="cz-input" type="text" value="default" required />
</div>
</div>

<div class="cz-setup-section">
<p class="cz-setup-section-title">Object storage</p>
<p class="cz-setup-section-hint">Where Lakekeeper writes Iceberg metadata + data files. By default this is the <strong>local Garage instance</strong> Computeza installs on this same host — the S3 protocol is just how Lakekeeper + Garage talk. Point at a remote S3 endpoint only if you've turned off the bundled Garage.</p>
<div class="cz-setup-field">
<label for="bs-s3-endpoint">Storage endpoint</label>
<input id="bs-s3-endpoint" name="s3_endpoint" class="cz-input" type="text" value="{s3_endpoint}" required />
<p class="cz-setup-field-hint">Default points at the local Garage on <code>127.0.0.1:3900</code>. No cloud egress, no AWS account needed.</p>
</div>
<div class="cz-setup-field">
<label for="bs-s3-region">Region</label>
<input id="bs-s3-region" name="s3_region" class="cz-input" type="text" value="garage" required />
<p class="cz-setup-field-hint">Default <code>garage</code> matches the local Garage's SigV4 default region. <code>us-east-1</code> here makes Garage reject Lakekeeper with <em>unexpected scope</em>. Match your Garage <code>s3_region</code> if explicitly set.</p>
</div>
<div class="cz-setup-field">
<label for="bs-s3-bucket">Bucket</label>
<input id="bs-s3-bucket" name="s3_bucket" class="cz-input" type="text" value="{prefill_bucket}" required />
<p class="cz-setup-field-hint">A bucket inside the local Garage; data stays on this host's disk.</p>
</div>
</div>

<div class="cz-setup-section">
<p class="cz-setup-section-title">Storage credentials</p>
<p class="cz-setup-section-hint">Local Garage signs every metadata write with these. Auto-minted from the Garage install — paste them only if pointing at a remote S3 endpoint.</p>
<div class="cz-setup-field">
<label for="bs-s3-ak">Access key ID</label>
<input id="bs-s3-ak" name="s3_access_key" class="cz-input" type="text" required placeholder="GK..." value="{prefill_key_id}" />
<p class="cz-setup-field-hint">The auto-generated Access Key ID (looks like <code>GK4cf9b7d2e9a4b78...</code>), <strong>not</strong> the human alias.</p>
<pre class="cz-setup-help"># installer ships garage as `computeza-garage` to avoid distro clashes
alias gg=&quot;sudo /usr/local/bin/computeza-garage -c /var/lib/computeza/garage/garage.toml&quot;

# one-time cluster layout (skip if `gg layout show` shows Status: active)
gg status                                      # copy local Node ID
gg layout assign &lt;node-id&gt; -z dc1 -c 10G
gg layout apply --version 1

# key + bucket + grant + read credentials
gg key create lakekeeper
gg bucket create lakekeeper-default
gg bucket allow lakekeeper-default --read --write --owner --key lakekeeper
gg key info lakekeeper                         # &lt;-- copy Key ID + Secret key</pre>
</div>
<div class="cz-setup-field">
<label for="bs-s3-sk">Secret access key</label>
<input id="bs-s3-sk" name="s3_secret_access_key" class="cz-input" type="password" required placeholder="opaque base64-ish string from `garage key info`" value="{prefill_secret}" />
</div>
</div>

<div class="cz-setup-actions">
<a href="/studio" class="cz-btn-ghost">Cancel</a>
<button type="submit" class="cz-btn-primary">Run bootstrap</button>
</div>
</form>

<div class="cz-setup-card" style="margin-top: 1.5rem; border-color: rgba(255, 157, 166, 0.3);">
<h2 style="margin-top: 0; font-size: 0.95rem; color: var(--fail);"><i class="fa-solid fa-triangle-exclamation"></i> Recovery: reset Lakekeeper state</h2>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.82rem; line-height: 1.55;">If bootstrap keeps failing with <code>NoSuchWarehouseException</code> or <code>WarehouseIdIsNotUUID</code>, the Lakekeeper postgres database has drifted from what Studio expects (orphaned warehouses from earlier attempts, duplicate names across projects, etc.). This button stops Lakekeeper, drops + recreates the <code>lakekeeper</code> postgres database, restarts the service, and clears the vault's cached warehouse identifiers. <strong>Destroys every warehouse and namespace</strong> in this Lakekeeper instance &mdash; the underlying Iceberg data files on Garage are NOT touched.</p>
<form method="post" action="/studio/bootstrap/reset" style="margin: 0;" onsubmit="return confirm('This destroys every warehouse + namespace + table metadata row in Lakekeeper. The on-disk Iceberg files in Garage stay put -- you can re-register the warehouse + namespaces and the table data remains queryable. Proceed?');">
{csrf}
<button type="submit" class="cz-btn-danger"><i class="fa-solid fa-rotate-left"></i> Reset Lakekeeper state</button>
</form>
</div>
</div>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        s3_endpoint = html_escape(suggested_s3_endpoint),
        prefill_bucket = html_escape(prefill_bucket),
        prefill_key_id = html_escape(&prefill.key_id),
        prefill_secret = html_escape(&prefill.secret),
        csrf = csrf,
        result_block = result_block,
    );

    render_shell(localizer, &title, NavLink::Studio, &body)
}

async fn install_hub_handler(State(state): State<AppState>) -> Response {
    let l = Localizer::english();
    let active = active_jobs(&state);
    // If an install is already in flight, auto-resume the wizard
    // instead of showing the picker. A page-refresh during install
    // used to dump the operator back to the form -- visually
    // identical to "start over" even though the background task was
    // still running -- so they'd hit Install again and double-fire.
    // Now the GET lands directly on /install/job/{id} where the
    // progress bars + poller pick up where they left off.
    if let Some(j) = active.first() {
        return Redirect303(format!("/install/job/{}", j.id)).into_response();
    }
    let installed = compute_installed_slugs(state.store.as_deref()).await;
    Html(render_install_hub(&l, &active, &installed)).into_response()
}

/// Read the metadata store and return the set of slugs that currently
/// have a `{slug}-instance/local` row -- i.e. the components the
/// operator has installed at least once and has not subsequently
/// uninstalled. Used by the install hub to render an Install button
/// (when missing) or an Uninstall button (when present) per card,
/// so operators can re-install or tear down a single component
/// without running the bulk Install-All / Uninstall-All flow.
///
/// Best-effort: a missing store or a list-failure returns an empty
/// set so the UI degrades to "show Install button everywhere" -- a
/// false negative is recoverable (the per-component handler is
/// idempotent) but a false positive (Uninstall on a fresh install)
/// would be confusing.
async fn compute_installed_slugs(
    store: Option<&computeza_state::SqliteStore>,
) -> std::collections::HashSet<String> {
    use computeza_state::Store;
    let Some(store) = store else {
        return std::collections::HashSet::new();
    };
    let mut out = std::collections::HashSet::new();
    for c in COMPONENTS {
        let kind = format!("{}-instance", c.slug);
        if let Ok(rows) = store.list(&kind, None).await {
            if !rows.is_empty() {
                out.insert(c.slug.to_string());
            }
        }
    }
    out
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
/// Auto-bootstrap Lakekeeper's default project + warehouse using
/// Garage credentials already in the vault. Returns artifacts to
/// be persisted (the warehouse name, for the studio catalog
/// browser to use).
///
/// Cross-component bootstrap: depends on Garage having installed
/// (and its post-install hook having populated
/// `garage/lakekeeper-key-id`, `garage/lakekeeper-secret`,
/// `garage/lakekeeper-bucket` in the vault). INSTALL_ORDER ensures
/// Garage installs first, so by the time this runs the vault
/// should have what we need.
#[cfg(target_os = "linux")]
async fn auto_bootstrap_lakekeeper(
    secrets: Option<&SecretsStore>,
    store: Option<&SqliteStore>,
) -> Result<Vec<computeza_driver_native::linux::BootstrapArtifact>, String> {
    use secrecy::{ExposeSecret, SecretString};
    let secrets = secrets.ok_or_else(|| {
        "no secrets store attached; auto-bootstrap requires the vault to read Garage credentials \
         from. Set COMPUTEZA_SECRETS_PASSPHRASE or let the install path auto-generate one."
            .to_string()
    })?;
    // SecretsStore::get returns Option<SecretString>; pull each
    // value into a plain String (via expose_secret) right here so
    // the rest of this function can build a StudioBootstrapForm
    // without the SecretString boxing in every call site. The
    // String lifetimes end inside run_lakekeeper_bootstrap below.
    let key_id: String = secrets
        .get("garage/lakekeeper-key-id")
        .await
        .map_err(|e| format!("vault read garage/lakekeeper-key-id: {e}"))?
        .ok_or_else(|| {
            "vault has no `garage/lakekeeper-key-id` -- Garage hasn't been installed yet, or its \
             post-install bootstrap failed. Install Garage from /install first.".to_string()
        })?
        .expose_secret()
        .to_string();
    let secret: String = secrets
        .get("garage/lakekeeper-secret")
        .await
        .map_err(|e| format!("vault read garage/lakekeeper-secret: {e}"))?
        .ok_or_else(|| {
            "vault has `garage/lakekeeper-key-id` but no `garage/lakekeeper-secret` -- state \
             mismatch. Rotate the Garage key (gg key delete lakekeeper && gg key create lakekeeper) \
             and re-install."
                .to_string()
        })?
        .expose_secret()
        .to_string();
    let bucket: String = secrets
        .get("garage/lakekeeper-bucket")
        .await
        .map_err(|e| format!("vault read garage/lakekeeper-bucket: {e}"))?
        .map(|s| s.expose_secret().to_string())
        .unwrap_or_else(|| "lakekeeper-default".to_string());

    let lakekeeper_url = discover_lakekeeper_endpoint(store).await.ok_or_else(|| {
        "no Lakekeeper instance registered in the metadata store; the install pipeline should \
         have written it. Check /status."
            .to_string()
    })?;
    let garage_endpoint = discover_garage_endpoint(store)
        .await
        .map(|u| u.replace(":3903", ":3900"))
        .unwrap_or_else(|| "http://127.0.0.1:3900".to_string());

    let form = StudioBootstrapForm {
        project_name: "computeza-default".to_string(),
        warehouse_name: "default".to_string(),
        s3_endpoint: garage_endpoint,
        s3_region: "garage".to_string(),
        s3_bucket: bucket.clone(),
        s3_access_key: key_id,
        s3_secret_access_key: secret,
    };
    let ok = run_lakekeeper_bootstrap(&lakekeeper_url, &form).await?;

    let mut artifacts = vec![computeza_driver_native::linux::BootstrapArtifact {
        vault_key: "lakekeeper/default-warehouse-name".to_string(),
        value: SecretString::from("default".to_string()),
        label: "Lakekeeper default warehouse name".to_string(),
        display_inline: false,
    }];
    if let Some(id) = ok.warehouse_id {
        artifacts.push(computeza_driver_native::linux::BootstrapArtifact {
            vault_key: "lakekeeper/default-warehouse-id".to_string(),
            value: SecretString::from(id),
            label: "Lakekeeper default warehouse UUID".to_string(),
            display_inline: false,
        });
    }
    Ok(artifacts)
}

/// POST /install/{slug}/retry-bootstrap — re-run the post-install
/// bootstrap step for a single component without doing a full
/// re-install. Use case: install completed cleanly (daemon up,
/// systemd unit registered, install-config persisted) but the
/// post-install bootstrap failed for a recoverable reason (network
/// blip, Lakekeeper API field-name drift the operator just patched,
/// vault unavailable at install time, etc.). Retry lets the
/// operator re-hit the bootstrap without going through the
/// uninstall + install cycle.
///
/// Returns an HTML page with the bootstrap result inlined.
/// Idempotent: the underlying bootstrap fns are designed to no-op
/// when state already exists.
async fn retry_bootstrap_handler(
    State(state): State<AppState>,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Html<String> {
    let l = Localizer::english();
    let result = run_post_install_bootstrap_for_slug(&slug, &state).await;
    let body = match result {
        Ok(summary) => format!(
            r#"<section class="cz-hero"><h1>Retry succeeded</h1></section>
<section class="cz-section" style="max-width: 50rem;">
<div class="cz-card" style="background: rgba(80, 220, 120, 0.06); border: 1px solid rgba(80, 220, 120, 0.3);">
<pre style="white-space: pre-wrap; margin: 0; font-size: 0.85rem;">{}</pre>
</div>
<p style="margin-top: 1rem;"><a href="/install" class="cz-btn">Back to Install hub</a> <a href="/studio" class="cz-btn">Open Studio</a></p>
</section>"#,
            html_escape(&summary)
        ),
        Err(msg) => format!(
            r#"<section class="cz-hero"><h1>Retry failed</h1></section>
<section class="cz-section" style="max-width: 50rem;">
<div class="cz-card" style="background: rgba(255, 99, 99, 0.06); border: 1px solid rgba(255, 99, 99, 0.3);">
<pre style="white-space: pre-wrap; margin: 0; font-size: 0.85rem;">{}</pre>
</div>
<p style="margin-top: 1rem;"><a href="/install" class="cz-btn">Back to Install hub</a></p>
</section>"#,
            html_escape(&msg)
        ),
    };
    Html(render_shell(
        &l,
        &format!("Retry bootstrap: {slug}"),
        NavLink::Install,
        &body,
    ))
}

/// Dispatcher: given a component slug, run that component's
/// post-install bootstrap step and persist any artifacts. Mirrors
/// the dispatch logic in `finalize_managed_install_after_success`
/// but factored out so the retry endpoint and the auto-run path
/// share one implementation.
async fn run_post_install_bootstrap_for_slug(
    slug: &str,
    state: &AppState,
) -> Result<String, String> {
    let store = state.store.as_deref();
    let secrets = state.secrets.as_deref();

    #[cfg(target_os = "linux")]
    {
        let config = match store {
            Some(s) => load_install_config(s, slug).await.unwrap_or_default(),
            None => InstallConfig::default(),
        };
        let bootstrap_result: Result<
            Vec<computeza_driver_native::linux::BootstrapArtifact>,
            String,
        > = match slug {
            "garage" => {
                let root = config
                    .root_dir
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/computeza/garage"));
                computeza_driver_native::linux::garage::post_install_bootstrap(&root)
                    .await
                    .map_err(|e| e.to_string())
            }
            "lakekeeper" => auto_bootstrap_lakekeeper(secrets, store).await,
            other => {
                return Err(format!(
                    "no post-install bootstrap defined for `{other}` -- only `garage` and `lakekeeper` support retry today. \
                     Add a branch in run_post_install_bootstrap_for_slug() if you've wired a new component."
                ));
            }
        };

        let artifacts = bootstrap_result?;
        let count = artifacts.len();
        for a in artifacts {
            use secrecy::ExposeSecret;
            let value = a.value.expose_secret().to_string();
            if let Some(secrets) = secrets {
                if let Err(e) = secrets.put(&a.vault_key, &value).await {
                    tracing::warn!(
                        error = %e,
                        component = slug,
                        vault_key = %a.vault_key,
                        "retry-bootstrap: secrets.put failed"
                    );
                }
            }
        }
        Ok(format!(
            "Retry succeeded for component `{slug}`. {count} artifact(s) persisted to vault.\n\nNext step: visit /studio to confirm the catalog state."
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (slug, store, secrets);
        Err("retry-bootstrap is only implemented on Linux".to_string())
    }
}

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

        // Apply the password to the running component BEFORE
        // persisting it in the secrets vault. Without this step the
        // vault would be the only place the password exists; the
        // component itself would still have its post-init default,
        // so reconcilers + operator logins would fail with auth
        // errors. Best-effort: a failure here is logged with the
        // exact recovery action and the install continues (the
        // operator can rotate via the admin UI to recover).
        if let Err(e) = apply_admin_password(slug, username, &password).await {
            tracing::warn!(
                error = %e,
                component = slug,
                "install: failed to apply generated admin password to the running \
                 {slug} component. The password is stored in secrets at \
                 `{secret_ref}` but the component still has its post-init default. \
                 Reconciler observe() will report FAILED until the operator rotates \
                 via POST /admin/secrets/{secret_ref}/rotate (or applies the password \
                 manually via the component's native CLI)."
            );
        }

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

    // Post-install bootstrap (v0.1 design doc §3.2). For components
    // that need post-daemon-start setup beyond the systemd unit,
    // their driver module exposes `post_install_bootstrap` returning
    // a Vec<BootstrapArtifact>. Each artifact is persisted into the
    // secrets vault under `vault_key`, and (if `display_inline`) is
    // also pushed to the install job's credentials list so the
    // credentials.json export downloadable from the install-result
    // page surfaces it. Failures are logged + surfaced inline on
    // the result page but do NOT fail the install -- the daemon is
    // running and registered, the bootstrap can be retried.
    #[cfg(target_os = "linux")]
    {
        let bootstrap_result: Option<
            Result<Vec<computeza_driver_native::linux::BootstrapArtifact>, String>,
        > = match slug {
            "garage" => {
                let root = config
                    .root_dir
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/computeza/garage"));
                Some(
                    computeza_driver_native::linux::garage::post_install_bootstrap(&root)
                        .await
                        .map_err(|e| e.to_string()),
                )
            }
            "lakekeeper" => {
                // Cross-component bootstrap: read Garage credentials
                // from vault (populated by `garage`'s earlier hook),
                // call the existing run_lakekeeper_bootstrap to
                // create the default project + warehouse. INSTALL_ORDER
                // ensures Garage installed first, so vault entries
                // should be populated by the time we hit this code.
                Some(auto_bootstrap_lakekeeper(secrets, store).await)
            }
            "trino" => {
                // Trino runs anonymous in v0.0.x -- the post-install
                // hook emits connection metadata (HTTP URL, JDBC URL,
                // default user) so the credentials.json export has
                // everything an external SQL client needs.
                let port = config.port.unwrap_or(
                    computeza_driver_native::linux::trino::DEFAULT_PORT,
                );
                Some(
                    computeza_driver_native::linux::trino::post_install_bootstrap(port)
                        .await
                        .map_err(|e| e.to_string()),
                )
            }
            _ => None,
        };
        if let Some(result) = bootstrap_result {
            match result {
                Ok(artifacts) => {
                    let count = artifacts.len();
                    for a in artifacts {
                        use secrecy::ExposeSecret;
                        let value = a.value.expose_secret().to_string();
                        if let Some(secrets) = secrets {
                            if let Err(e) = secrets.put(&a.vault_key, &value).await {
                                tracing::warn!(
                                    error = %e,
                                    component = slug,
                                    vault_key = %a.vault_key,
                                    "post-install bootstrap: secrets.put failed; \
                                     artifact still appears in the credentials.json download."
                                );
                            }
                        }
                        if a.display_inline {
                            progress.push_credential(
                                computeza_driver_native::progress::GeneratedCredential {
                                    component: slug.to_string(),
                                    label: a.label,
                                    value,
                                    username: None,
                                    secret_ref: Some(a.vault_key),
                                },
                            );
                        }
                    }
                    if count > 0 {
                        summary.push_str(&format!(
                            "\n\nPost-install bootstrap for {slug}: {count} artifact(s) persisted to vault."
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        component = slug,
                        "post-install bootstrap failed; daemon is running but downstream \
                         auto-configuration may not work. Operator can retry via the studio \
                         bootstrap form."
                    );
                    summary.push_str(&format!(
                        "\n\nNote: post-install bootstrap for {slug} failed:\n{e}\n\nThe daemon is running and registered. Retry the bootstrap from /studio/bootstrap."
                    ));
                }
            }
        }
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

/// Apply a freshly-generated admin password to a running component so
/// the reconciler + operator's stored credential agree with what the
/// component will accept.
///
/// Currently wired for postgres only. Pipes
/// `ALTER USER <user> PASSWORD '<pw>';` to `sudo -u postgres psql`
/// via stdin (not -c) so the password does NOT appear in `ps` output.
/// The psql invocation goes over the local Unix socket where the
/// install's initdb left `--auth-local=peer`, so no bootstrap
/// password is required.
///
/// kanidm + grafana password-apply paths land in a follow-up commit:
/// each has its own mechanism (kanidm via `kanidmd recover_account`;
/// grafana via the admin HTTP API), and wiring them is a separate
/// concern from the postgres reconciler unblock.
async fn apply_admin_password(slug: &str, username: &str, password: &str) -> Result<(), String> {
    match slug {
        "postgres" => apply_postgres_password(username, password).await,
        "kanidm" | "grafana" => {
            tracing::debug!(
                component = slug,
                "no admin-password applier wired for {slug} yet; the generated \
                 password is in secrets, but the component still has its post-init \
                 default. Manual rotation via the component's CLI is required for \
                 the reconciler / operator login to succeed."
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Pipe `ALTER USER ... PASSWORD ...` to `sudo -u postgres psql` via
/// stdin. Password reaches psql via the child process's stdin pipe,
/// never via the command line, so `ps` does NOT leak it.
///
/// Returns the stderr / exit-code on failure so the caller's
/// `tracing::warn!` carries actionable diagnostics.
async fn apply_postgres_password(username: &str, password: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    let mut child = Command::new("sudo")
        .arg("-n") // no password prompt; the install runs as root already
        .arg("-u")
        .arg("postgres")
        .arg("psql")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("--quiet")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning `sudo -u postgres psql`: {e}"))?;

    let sql = format!("ALTER USER \"{username}\" PASSWORD '{password}';\n");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(sql.as_bytes())
            .await
            .map_err(|e| format!("writing ALTER USER SQL to psql stdin: {e}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| format!("closing psql stdin: {e}"))?;
    }

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("waiting on psql: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "psql exited {:?}: stdout={} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
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
    // Parse optional IdP binding fields. When idp_kind is unset
    // (operator left the dropdown on "None"), no IdpConfig is
    // attached and federation is skipped for this component.
    let idp_kind_raw = form
        .get(&format!("{slug}__idp_kind"))
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    let idp_config = if idp_kind_raw.is_empty() {
        None
    } else {
        use computeza_identity_federation::IdpKind;
        let kind = match idp_kind_raw {
            "entra-id" => IdpKind::EntraId,
            "aws-iam" => IdpKind::AwsIam,
            "gcp-iam" => IdpKind::GcpIam,
            "keycloak" => IdpKind::Keycloak,
            "generic-oidc" => IdpKind::GenericOidc,
            other => return Err(format!("{slug} unknown IdP kind: {other:?}")),
        };
        let get_str = |k: &str| -> String {
            form.get(&format!("{slug}__{k}"))
                .map(String::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let cfg = computeza_identity_federation::IdpConfig {
            kind,
            discovery_url: get_str("idp_discovery_url"),
            client_id: get_str("idp_client_id"),
            client_secret_ref: {
                let r = get_str("idp_secret_ref");
                if r.is_empty() {
                    None
                } else {
                    Some(r)
                }
            },
            redirect_uri: get_str("idp_redirect_uri"),
            claim_mappings: Vec::new(),
        };
        cfg.validate()
            .map_err(|e| format!("{slug} IdP config: {e}"))?;
        Some(cfg)
    };

    Ok(InstallConfig {
        version,
        port,
        root_dir,
        service_name,
        idp_config,
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
            "sail" => {
                let mut opts = linux::sail::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::sail::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "trino" => {
                let mut opts = linux::trino::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::trino::uninstall(opts)
                    .await
                    .map(|r| format_uninstall_summary(&r.steps, &r.warnings))
                    .map_err(|e| format!("{e}"))
            }
            "xtable" => {
                let mut opts = linux::xtable::UninstallOptions::default();
                apply_uninstall_config_overrides(config, &mut opts.unit_name, &mut opts.root_dir);
                linux::xtable::uninstall(opts)
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
        "trino" => run_trino_install_with_progress(progress, config).await?,
        "sail" => run_sail_install_with_progress(progress, config).await?,
        "xtable" => run_xtable_install_with_progress(progress, config).await?,
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
    /// Optional upstream IdP binding. Persisted alongside the spec
    /// so the kanidm federation reconciler (v0.1+) can consume it
    /// without an additional store lookup. `None` for components
    /// the operator chose to leave on local-only auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idp_config: Option<computeza_identity_federation::IdpConfig>,
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
            // Per-component install handlers don't yet expose the
            // IdP form. The unified install at /install does;
            // operators wanting federation on a per-component path
            // can install via /install and the IdP config persists
            // with the install-config row.
            idp_config: None,
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
// Sail install path -- pip-into-venv + Spark Connect gRPC
// ============================================================

#[cfg(target_os = "linux")]
async fn run_sail_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::sail;
    let mut opts = sail::InstallOptions::default();
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
    match sail::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "venv bin_dir: {}\nunit_path: {}\nSpark Connect (gRPC) port: {}\nClient URI: sc://127.0.0.1:{}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_sail_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Sail install requires a supported Linux host.".into())
}

// ============================================================
// Trino install path -- tarball + bundled Temurin JRE 21
// ============================================================

#[cfg(target_os = "linux")]
async fn run_trino_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::trino;
    let mut opts = trino::InstallOptions::default();
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
    match trino::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "trino bin_dir: {}\nunit_path: {}\ncoordinator HTTP port: {}\nClient URL: http://127.0.0.1:{}",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.port,
                r.port,
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_trino_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("Trino install requires a supported Linux host.".into())
}

// ============================================================
// xtable install path -- Maven-resolve + bundled Temurin JRE
// ============================================================

#[cfg(target_os = "linux")]
async fn run_xtable_install_with_progress(
    progress: &ProgressHandle,
    config: &InstallConfig,
) -> Result<(String, u16), String> {
    use computeza_driver_native::linux::xtable;
    let mut opts = xtable::InstallOptions::default();
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
    match xtable::install(opts, progress).await {
        Ok(r) => Ok((
            format!(
                "lib_dir: {}\nunit_path: {}\ndatasets.yaml (operator-supplied at next start): {}/datasets.yaml",
                r.bin_dir.display(),
                r.unit_path.display(),
                r.bin_dir
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown)".into()),
            ),
            r.port,
        )),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_xtable_install_with_progress(
    _progress: &ProgressHandle,
    _config: &InstallConfig,
) -> Result<(String, u16), String> {
    Err("xtable install requires a supported Linux host.".into())
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
        Ok(summary) => render_uninstall_result(&l, true, &summary),
        Err(detail) => render_uninstall_result(&l, false, &detail),
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
///
/// - Windows: EnterpriseDB still publishes portable ZIPs, so the
///   driver downloads them per-release; the dropdown lists every
///   pinned bundle.
/// - Linux: EnterpriseDB stopped publishing Linux tarballs around
///   2023. The driver falls through to the host package manager
///   (apt / dnf / zypper / pacman); the dropdown lists the majors
///   we'll request, with "distro-default" mapping to the
///   unversioned meta-package (recommended).
/// - macOS: still uses Postgres.app / brew detection.
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
    #[cfg(target_os = "linux")]
    {
        use computeza_driver_native::linux::postgres;
        postgres::available_majors()
            .iter()
            .map(|m| {
                if *m == "distro-default" {
                    VersionOption {
                        value: String::new(),
                        label: "distro default (recommended)".into(),
                    }
                } else {
                    VersionOption {
                        value: (*m).to_string(),
                        label: format!("PostgreSQL {m}"),
                    }
                }
            })
            .collect()
    }
    #[cfg(target_os = "macos")]
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
            //
            // Stash a clone under `credentials_for_download` so a
            // single follow-up `GET /install/credentials.json/{job_id}`
            // can return the same bag as a one-shot JSON download.
            // Stashing only on first render (when the field is None)
            // means a refresh of the page won't re-expose
            // credentials via the download URL either.
            let credentials = job_arc
                .as_ref()
                .map(|s| {
                    let mut guard = s.lock().unwrap();
                    let bag = std::mem::take(&mut guard.generated_credentials);
                    if guard.credentials_for_download.is_none() && !bag.is_empty() {
                        guard.credentials_for_download = Some(bag.clone());
                    }
                    bag
                })
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

            // The download URL is only useful when we have
            // credentials stashed AND we know the job_id (always
            // true on this path). Hide the button on an empty bag.
            let download_id = if credentials.is_empty() {
                None
            } else {
                Some(job_id.as_str())
            };
            // On the rollback flow, render_uninstall_result swaps
            // "Install completed" / "Install failed" labels for the
            // "Uninstall completed / with errors" variants. No
            // rollback-button on a rollback result page (would be a
            // rollback of a rollback, which v0.0.x doesn't support).
            if let Some(err) = &p.error {
                if p.is_rollback {
                    (StatusCode::OK, Html(render_uninstall_result(&l, false, err)))
                } else {
                    (
                        StatusCode::OK,
                        Html(render_install_result_with_credentials(
                            &l,
                            false,
                            err,
                            &credentials,
                            rollback_id,
                            download_id,
                        )),
                    )
                }
            } else {
                let summary = p.success_summary.clone().unwrap_or_default();
                if p.is_rollback {
                    (StatusCode::OK, Html(render_uninstall_result(&l, true, &summary)))
                } else {
                    (
                        StatusCode::OK,
                        Html(render_install_result_with_credentials(
                            &l,
                            true,
                            &summary,
                            &credentials,
                            rollback_id,
                            download_id,
                        )),
                    )
                }
            }
        }
        Some(p) => (
            StatusCode::OK,
            Html(render_install_progress(&l, &job_id, &p)),
        ),
    }
}

/// GET /install/credentials.json/{job_id} -- one-shot JSON download
/// of every credential generated during the install job. Drains
/// `credentials_for_download` on first call so the download is
/// truly view-once (matches the on-page table's view-once
/// contract); subsequent calls return 410 Gone.
///
/// Gated behind `Permission::Manage` by `required_permission_for` --
/// a Viewer should never reach a payload of admin passwords.
///
/// Every successful download writes an audit-log entry so a future
/// breach investigation can answer "which operator pulled the
/// credentials, when". The audit entry records WHO downloaded and
/// WHICH job, not the credentials themselves.
async fn install_credentials_json_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    axum::Extension(session): axum::Extension<auth::Session>,
) -> Response {
    let job_arc = state.jobs.lock().unwrap().get(&job_id).cloned();
    let Some(job_arc) = job_arc else {
        return (StatusCode::NOT_FOUND, "unknown install job").into_response();
    };
    let drained = {
        let mut guard = job_arc.lock().unwrap();
        guard.credentials_for_download.take()
    };
    let Some(creds) = drained else {
        // Either the result page hasn't rendered yet OR the file
        // has already been downloaded. Either way the bag is gone.
        return (
            StatusCode::GONE,
            "credentials are no longer available -- either the install hasn't completed yet, or this file has already been downloaded once. Each install run discloses its generated credentials exactly once; rotate them via /admin/secrets if you need to recover.",
        )
            .into_response();
    };

    let entries: Vec<serde_json::Value> = creds
        .iter()
        .map(|c| {
            serde_json::json!({
                "component": c.component,
                "label": c.label,
                "username": c.username,
                "value": c.value,
                "secret_ref": c.secret_ref,
            })
        })
        .collect();

    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let now = chrono::Utc::now();
    let body = serde_json::json!({
        "install_job_id": job_id,
        "generated_at": now.to_rfc3339(),
        "host": host,
        "warning": "Treat this file as a secrets bundle. Anyone holding it can log in to every listed component. Store in a password manager or vault, then delete this download. Credentials can be rotated under /admin/secrets when the secrets store is attached.",
        "credentials": entries,
    });
    let json = match serde_json::to_vec_pretty(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "serializing install credentials JSON failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "credentials JSON serialization failed",
            )
                .into_response();
        }
    };

    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("install-job/{job_id}/credentials.json")),
                serde_json::json!({
                    "action": "download_install_credentials_json",
                    "install_job_id": job_id,
                    "credential_count": creds.len(),
                }),
            )
            .await;
    }

    let filename = format!(
        "computeza-credentials-{}.json",
        now.format("%Y%m%dT%H%M%SZ")
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, json).into_response()
}

/// GET /admin/secrets -- list every secret in the encrypted store.
/// Names only; values stay encrypted on disk and are never surfaced
/// from this page. Per-row Rotate button posts to
/// `/admin/secrets/{name}/rotate` which replaces the value and shows
/// the new value exactly once. When no store is attached
/// (COMPUTEZA_SECRETS_PASSPHRASE unset), the page surfaces a first-
/// boot wizard so the operator can generate a passphrase and copy
/// it into a systemd drop-in.
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

/// POST /admin/secrets/setup/generate-passphrase -- generate a 256-bit
/// CSPRNG passphrase (32 random bytes, hex-encoded), display it once
/// alongside the systemd drop-in template the operator needs to wire
/// it into `computeza serve`. The passphrase is **not** persisted by
/// Computeza -- the operator must copy it out before navigating away
/// (and back up alongside the salt + ciphertext files per the
/// disaster-recovery rule in [`computeza_secrets`]).
async fn secrets_setup_generate_handler() -> Response {
    let passphrase = generate_passphrase_hex();
    let l = Localizer::english();
    Html(render_secrets_setup_generated(&l, &passphrase)).into_response()
}

/// 64-char hex passphrase backed by 256 bits of CSPRNG entropy.
/// Hex keeps the value ASCII-only so it survives every shell quoting
/// rule without re-encoding -- safe to paste into a systemd
/// EnvironmentFile, a `.env` file, or `export FOO=...`.
fn generate_passphrase_hex() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    use aes_gcm::aead::OsRng;
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(64);
    for b in &buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
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

    // If this secret is a managed-component admin password
    // (`{slug}/admin-password` where slug is in
    // COMPONENTS_WITH_ADMIN_CREDENTIAL), apply the new value to the
    // running component BEFORE persisting it to the vault. Mirrors the
    // install path's apply-then-persist ordering so a rotation failure
    // leaves the vault and the live component in agreement (vault keeps
    // the old value, component keeps the old value). If we persisted
    // first and the apply failed, the next reconciler observe would
    // start failing -- exactly the bug postgres just hit after the
    // unguarded rotate that motivated this fix.
    if let Some((slug, username, _label)) = COMPONENTS_WITH_ADMIN_CREDENTIAL
        .iter()
        .find(|(s, _, _)| name == format!("{s}/admin-password"))
        .copied()
    {
        if let Err(e) = apply_admin_password(slug, username, &new_value).await {
            return Html(render_install_result(
                &l,
                false,
                &format!(
                    "Rotating {name}: generated a new password but FAILED to apply it to \
                     the running {slug} component ({e}). Vault still holds the previous \
                     value; the live component is unchanged. Investigate the error above \
                     (typically: the component is not running, sudo is unavailable, or \
                     the component's CLI changed). Re-run rotation after fixing."
                ),
            ))
            .into_response();
        }
    }

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
    // Snapshot the source job to figure out which components are
    // installed -- the order we tear them down in mirrors that
    // job's install order, reversed.
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

    // Spawn a fresh background job for the rollback so the operator
    // gets the same progress-bar wizard UX as install: per-component
    // pending/running/done states, polling /api/install/job/{id}
    // every 500ms, redirect to result page on completion. Returns
    // the new job_id so the 303 lands the operator on the live
    // progress page instead of waiting for the full sync teardown.
    use computeza_driver_native::progress::{ComponentProgress, ComponentState, InstallPhase};
    let new_job_id = uuid::Uuid::new_v4().to_string();
    let teardown_order: Vec<String> = installed.iter().rev().cloned().collect();
    let progress_arc = std::sync::Arc::new(std::sync::Mutex::new(
        computeza_driver_native::progress::InstallProgress {
            phase: InstallPhase::Queued,
            message: format!("Rolling back {} components", teardown_order.len()),
            log: Vec::new(),
            components: teardown_order
                .iter()
                .map(|slug| ComponentProgress {
                    slug: slug.clone(),
                    state: ComponentState::Pending,
                    error: None,
                    summary: None,
                })
                .collect(),
            is_rollback: true,
            ..Default::default()
        },
    ));
    state
        .jobs
        .lock()
        .unwrap()
        .insert(new_job_id.clone(), progress_arc.clone());

    let store = state.store.clone();
    let secrets = state.secrets.clone();
    let progress = progress_arc.clone();
    tokio::spawn(async move {
        let mut summary = String::new();
        let mut any_failed = false;
        for slug in &teardown_order {
            // Mark this component as Running so the wizard's
            // checklist shows the live cursor.
            {
                let mut p = progress.lock().unwrap();
                p.phase = InstallPhase::StartingService;
                p.message = format!("Uninstalling {slug}");
                p.log.push(format!("=== uninstall: {slug} ==="));
                if let Some(c) = p.components.iter_mut().find(|c| c.slug == *slug) {
                    c.state = ComponentState::Running;
                }
            }
            summary.push_str(&format!("=== uninstall: {slug} ===\n"));

            let config = if let Some(store) = &store {
                load_install_config(store.as_ref(), slug)
                    .await
                    .unwrap_or_default()
            } else {
                InstallConfig::default()
            };

            let mut step_failed = false;
            match dispatch_uninstall_with_config(slug, &config).await {
                Ok(detail) => {
                    summary.push_str(&format!("{detail}\n\n"));
                    progress.lock().unwrap().log.push(detail);
                }
                Err(e) => {
                    any_failed = true;
                    step_failed = true;
                    summary.push_str(&format!("FAIL  {e}\n\n"));
                    progress.lock().unwrap().log.push(format!("FAIL: {e}"));
                    tracing::warn!(component = %slug, error = %e, "rollback: dispatch_uninstall_with_config failed; continuing");
                }
            }
            if let Some(store) = &store {
                let kind = format!("{slug}-instance");
                let key = ResourceKey::cluster_scoped(&kind, "local");
                if let Err(e) = store.delete(&key, None).await {
                    tracing::warn!(error = %e, component = %slug, "rollback: store.delete failed");
                    summary.push_str(&format!(
                        "Note: failed to drop {slug}-instance/local from metadata store ({e}).\n\n"
                    ));
                }
                delete_install_config(store.as_ref(), slug).await;
            }
            if let Some(secrets) = &secrets {
                let secret_ref = format!("{slug}/admin-password");
                let _ = secrets.delete(&secret_ref).await;
            }
            // Mark Done (or Failed if dispatch errored).
            {
                let mut p = progress.lock().unwrap();
                if let Some(c) = p.components.iter_mut().find(|c| c.slug == *slug) {
                    c.state = if step_failed {
                        ComponentState::Failed
                    } else {
                        ComponentState::Done
                    };
                }
            }
        }

        let final_summary = if any_failed {
            format!("Uninstall completed with errors. See per-component log below.\n\n{summary}")
        } else {
            format!(
                "Uninstall and removal of components completed.\n\n\
                 {n} component(s) torn down in reverse dependency order. \
                 Per-component details below.\n\n{summary}",
                n = teardown_order.len()
            )
        };
        let mut p = progress.lock().unwrap();
        p.phase = if any_failed {
            InstallPhase::Failed
        } else {
            InstallPhase::Done
        };
        p.message = if any_failed {
            "Uninstall completed with errors".into()
        } else {
            "Uninstall and removal of components completed".into()
        };
        p.completed = true;
        p.finished_at = Some(chrono::Utc::now());
        if any_failed {
            p.error = Some(final_summary);
        } else {
            p.success_summary = Some(final_summary);
        }
    });

    Redirect303(format!("/install/job/{new_job_id}")).into_response()
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
/// `progress` streams per-phase status on every platform. The Linux
/// path uses it for the EDB-tarball fetch + the subsequent initdb /
/// systemd phases; macOS still passes `_progress` as a no-op handle.
#[cfg(target_os = "linux")]
async fn run_postgres_install_with_progress(
    progress: &ProgressHandle,
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
    if let Some(v) = &config.version {
        opts.version = Some(v.clone());
    }
    match postgres::install_with_progress(opts, progress).await {
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
        "trino-instance",
        "sail-instance",
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

/// GET /install-guide -- public step-by-step setup reference. Public
/// so a prospective buyer can read the prereqs + commands before
/// signing in. Static content; no per-tenant data touched.
async fn install_guide_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_guide(&l))
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
        ("trino-instance", "trino"),
        ("sail-instance", "sail"),
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
        ("trino-instance", "trino"),
        ("sail-instance", "sail"),
        ("qdrant-instance", "qdrant"),
        ("restate-instance", "restate"),
        ("greptime-instance", "greptime"),
        ("grafana-instance", "grafana"),
        ("openfga-instance", "openfga"),
    ];
    let mut rows: Vec<StatusRow> = Vec::new();
    for (kind, slug) in entries {
        let component_label = l.t(&format!("component-{slug}-name"));
        let Ok(list) = store.list(kind, None).await else {
            continue;
        };
        if list.is_empty() {
            // No instance row for this component yet -- still surface
            // a placeholder so /status shows the full catalogue of
            // managed components at a glance. Empty `instance_name`
            // is the sentinel render_status uses to render the
            // "Not installed" badge instead of a dead resource link.
            rows.push(StatusRow {
                kind: (*kind).into(),
                component_label,
                instance_name: String::new(),
                server_version: None,
                last_observed_at: None,
                last_observe_failed: false,
                has_status: false,
            });
            continue;
        }
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
                component_label: component_label.clone(),
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
                "trino-instance",
                "sail-instance",
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
    /// Group memberships. Submitted as one `groups=...` pair per
    /// checked checkbox in the form; we accept either that
    /// (`["admins","operators"]`) or a legacy comma-separated single
    /// string (`["admins,operators"]`) and flatten in the handler.
    #[serde(default)]
    groups: Vec<String>,
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
    // Accept both shapes -- modern multi-value (one pair per
    // checkbox) and legacy comma-separated single string. Both
    // flatten into a clean Vec<String> here.
    let groups: Vec<String> = form
        .groups
        .iter()
        .flat_map(|g| g.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let groups = if groups.is_empty() {
        vec!["operators".to_string()]
    } else {
        groups
    };
    // Seat-cap enforcement: when the active license carries a seat
    // count, the live operator count must stay <= cap. We count
    // BEFORE create so the limit holds even under rapid double-
    // submits. Enterprise licenses (seats=None) always pass through.
    if let Some(lic) = state.license.read().await.as_ref() {
        if let Some(cap) = lic.payload.seats {
            let current = operators.list().await.len() as u32;
            if current >= cap {
                let list = operators.list().await;
                return Html(render_admin_operators(
                    &l,
                    &list,
                    &session.username,
                    Some(&format!(
                        "Seat cap reached: this license entitles {cap} operator(s) and {current} \
                         is/are already provisioned. Contact your reseller / Computeza sales to \
                         expand the entitlement, or remove an existing operator first."
                    )),
                ))
                .into_response();
            }
        }
    }
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
    /// Same shape as [`CreateOperatorForm::groups`] -- one entry
    /// per checked checkbox in the form, or a single legacy comma-
    /// separated string. Flattened in the handler.
    #[serde(default)]
    groups: Vec<String>,
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
        .iter()
        .flat_map(|g| g.split(','))
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

/// GET /admin/studios -- list studios. v0.0.x always shows
/// the implicit `default` studio; v0.1+ wires creation + per-
/// tenant resource scoping.
async fn admin_tenants_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_admin_studios(&l))
}

/// GET /admin/branding -- the white-labeling page. Reads the
/// current accent color from the install-config row under
/// `branding/default` (if any) and surfaces it on the form.
async fn admin_branding_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let accent = current_accent_color(&state).await;
    Html(render_admin_branding(&l, accent.as_deref(), None))
}

#[derive(serde::Deserialize)]
struct BrandingForm {
    accent: String,
}

/// POST /admin/branding -- persist the accent color. v0.0.x stores
/// it in a dedicated state-store row under `branding/default`;
/// v0.1+ extends to a full tenant theme.
async fn admin_branding_save_handler(
    State(state): State<AppState>,
    Form(form): Form<BrandingForm>,
) -> Response {
    let l = Localizer::english();
    let accent_trimmed = form.accent.trim().to_string();
    if !is_valid_hex_color(&accent_trimmed) && !accent_trimmed.is_empty() {
        return Html(render_admin_branding(
            &l,
            Some(&accent_trimmed),
            Some("Accent must be a hex color like #C4B8E8 or empty to reset."),
        ))
        .into_response();
    }
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("branding", "default");
        let value = serde_json::json!({"accent_color": accent_trimmed});
        let expected = match store.load(&key).await {
            Ok(Some(existing)) => Some(existing.revision),
            _ => None,
        };
        if let Err(e) = store.save(&key, &value, expected).await {
            tracing::warn!(error = %e, "admin_branding_save_handler: store.save failed");
            return Html(render_admin_branding(
                &l,
                Some(&accent_trimmed),
                Some(&format!("Saving branding failed: {e}")),
            ))
            .into_response();
        }
    }
    Redirect303("/admin/branding".into()).into_response()
}

/// Validate a hex color like `#C4B8E8` or `#fff`. Loose -- we
/// accept `#abc` and `#aabbcc`; v0.1+ extends to rgba / hsl when
/// the palette form grows.
fn is_valid_hex_color(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('#') else {
        return false;
    };
    matches!(rest.len(), 3 | 6) && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// Read the persisted accent color from the metadata store. Returns
/// `None` when no branding row exists or the store is detached.
async fn current_accent_color(state: &AppState) -> Option<String> {
    let store = state.store.as_ref()?;
    let key = ResourceKey::cluster_scoped("branding", "default");
    let stored = store.load(&key).await.ok().flatten()?;
    stored
        .spec
        .get("accent_color")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// GET /admin/license -- render the live license envelope (or the
/// Community-mode card when none is activated). Includes the
/// activation form so an operator can paste a freshly-issued envelope
/// and the deactivation form when one is currently active.
async fn admin_license_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let lic = state.license.read().await.clone();
    let status = state.license_status().await;
    let seat_usage = if matches!(lic, Some(ref l) if l.payload.seats.is_some()) {
        Some(state.operator_count().await)
    } else {
        None
    };
    Html(render_admin_license_v2(
        &l,
        lic.as_ref(),
        status,
        seat_usage,
        None,
    ))
}

/// POST /admin/license/activate -- accept a pasted license envelope,
/// verify against the trusted root + current time, persist to disk,
/// and reload state.
async fn admin_license_activate_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Form(form): Form<ActivateLicenseForm>,
) -> Response {
    let l = Localizer::english();
    let trimmed = form.envelope.trim();
    if trimmed.is_empty() {
        let lic = state.license.read().await.clone();
        let status = state.license_status().await;
        return Html(render_admin_license_v2(
            &l,
            lic.as_ref(),
            status,
            None,
            Some("Paste a license envelope before submitting."),
        ))
        .into_response();
    }
    let license = match computeza_license::parse_envelope(trimmed) {
        Ok(license) => license,
        Err(e) => {
            let lic = state.license.read().await.clone();
            let status = state.license_status().await;
            return Html(render_admin_license_v2(
                &l,
                lic.as_ref(),
                status,
                None,
                Some(&format!("Envelope did not parse as JSON: {e}")),
            ))
            .into_response();
        }
    };
    // Verify against the binary's trusted root + current time before
    // persisting. We do NOT accept envelopes that fail verification --
    // a bad license is worse than no license (operator thinks they
    // have entitlements they don't).
    let root = computeza_license::trusted_root();
    if let Err(e) = license.verify(Some(&root), chrono::Utc::now()) {
        let lic = state.license.read().await.clone();
        let status = state.license_status().await;
        return Html(render_admin_license_v2(
            &l,
            lic.as_ref(),
            status,
            None,
            Some(&format!("License verification failed: {e}")),
        ))
        .into_response();
    }
    // Persist.
    let Some(path) = state.license_path.clone() else {
        // Smoke-test surface: no path configured. Store in memory
        // only (won't survive restart but lets tests exercise the
        // activation path).
        *state.license.write().await = Some(license.clone());
        return Redirect303("/admin/license".into()).into_response();
    };
    if let Err(e) = computeza_license::save_license_file(&path, &license) {
        let lic = state.license.read().await.clone();
        let status = state.license_status().await;
        return Html(render_admin_license_v2(
            &l,
            lic.as_ref(),
            status,
            None,
            Some(&format!("Could not write {}: {e}", path.display())),
        ))
        .into_response();
    }
    let license_id = license.payload.id.clone();
    let tier = license.payload.tier.clone();
    let seats = license.payload.seats;
    *state.license.write().await = Some(license);
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("license/{license_id}")),
                serde_json::json!({
                    "action": "activate_license",
                    "license_id": license_id,
                    "tier": tier,
                    "seats": seats,
                }),
            )
            .await;
    }
    Redirect303("/admin/license".into()).into_response()
}

#[derive(serde::Deserialize)]
struct ActivateLicenseForm {
    envelope: String,
}

/// POST /admin/license/deactivate -- remove the on-disk envelope and
/// fall back to Community mode. Idempotent.
async fn admin_license_deactivate_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
) -> Response {
    let prior_id = state
        .license
        .read()
        .await
        .as_ref()
        .map(|l| l.payload.id.clone());
    if let Some(path) = state.license_path.clone() {
        let _ = computeza_license::delete_license_file(&path);
    }
    *state.license.write().await = None;
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                prior_id.as_ref().map(|id| format!("license/{id}")),
                serde_json::json!({
                    "action": "deactivate_license",
                    "license_id": prior_id,
                }),
            )
            .await;
    }
    Redirect303("/admin/license".into()).into_response()
}

/// GET /compliance/eu-ai-act -- overview of the deployer's
/// obligations under the EU AI Act (Regulation (EU) 2024/1689).
/// Renders the Article 9-72 checklist; cross-links to the model
/// card registry so the deployer can attach evidence per article.
async fn compliance_eu_ai_act_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_compliance_eu_ai_act(&l))
}

/// GET /compliance/models -- list every registered model card.
async fn compliance_models_list_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let cards = match &state.model_cards {
        Some(r) => r.list().await,
        None => Vec::new(),
    };
    Html(render_compliance_models_list(
        &l,
        &cards,
        state.model_cards.is_some(),
        None,
    ))
}

#[derive(serde::Deserialize)]
struct CreateModelCardForm {
    id: String,
    name: String,
    risk: String,
    rationale: String,
    intended_use: String,
    training_data_summary: String,
    limitations: String,
    human_oversight_design: String,
}

/// POST /compliance/models -- register a fresh model card.
async fn compliance_models_create_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Form(form): Form<CreateModelCardForm>,
) -> Response {
    let l = Localizer::english();
    let Some(registry) = &state.model_cards else {
        return Html(render_compliance_models_list(
            &l,
            &[],
            false,
            Some(
                "No model-card registry attached on this server. The smoke-test surface does not persist cards.",
            ),
        ))
        .into_response();
    };
    let risk = match form.risk.as_str() {
        "high-risk" => computeza_compliance::RiskClassification::HighRisk,
        "limited-risk" => computeza_compliance::RiskClassification::LimitedRisk,
        "minimal" => computeza_compliance::RiskClassification::Minimal,
        "prohibited" => {
            // Reject at the form-handler too -- the registry would
            // reject anyway, this surfaces the message earlier.
            let cards = registry.list().await;
            return Html(render_compliance_models_list(
                &l,
                &cards,
                true,
                Some(
                    "Prohibited classifications (Article 5 / Title II) cannot be registered. Remove the system from your AI studio before continuing.",
                ),
            ))
            .into_response();
        }
        other => {
            let cards = registry.list().await;
            return Html(render_compliance_models_list(
                &l,
                &cards,
                true,
                Some(&format!(
                    "Unknown risk classification: {other}. Expected one of: high-risk / limited-risk / minimal."
                )),
            ))
            .into_response();
        }
    };
    let now = chrono::Utc::now();
    let card = computeza_compliance::ModelCard {
        id: form.id.trim().to_string(),
        name: form.name.trim().to_string(),
        risk,
        risk_justification: computeza_compliance::RiskJustification {
            rationale: form.rationale,
            citations: Vec::new(),
        },
        intended_use: form.intended_use,
        training_data_summary: form.training_data_summary,
        limitations: form.limitations,
        human_oversight_design: form.human_oversight_design,
        evaluation_metrics: Vec::new(),
        deployments: Vec::new(),
        article_evidence: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = registry.create(card.clone()).await {
        let cards = registry.list().await;
        return Html(render_compliance_models_list(
            &l,
            &cards,
            true,
            Some(&e.to_string()),
        ))
        .into_response();
    }
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("model-card/{}", card.id)),
                serde_json::json!({
                    "action": "register_model_card",
                    "id": card.id,
                    "risk": card.risk.slug(),
                }),
            )
            .await;
    }
    Redirect303(format!("/compliance/models/{}", urlencoding_min(&card.id))).into_response()
}

/// GET /compliance/models/{id} -- single model card view.
async fn compliance_models_detail_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let l = Localizer::english();
    let Some(registry) = &state.model_cards else {
        return (
            StatusCode::NOT_FOUND,
            Html(render_compliance_models_list(
                &l,
                &[],
                false,
                Some("No model-card registry attached on this server."),
            )),
        )
            .into_response();
    };
    match registry.get(&id).await {
        Some(card) => Html(render_compliance_model_detail(&l, &card)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<!DOCTYPE html><html><body style=\"font-family:sans-serif;padding:2rem;\"><h1>Model card not found</h1><p>No card registered with id <code>{}</code>.</p><p><a href=\"/compliance/models\">Back to registry</a></p></body></html>",
                html_escape(&id),
            )),
        )
            .into_response(),
    }
}

/// POST /compliance/models/{id}/delete -- remove a card from the
/// registry. Audit-logged.
async fn compliance_models_delete_handler(
    State(state): State<AppState>,
    axum::Extension(session): axum::Extension<auth::Session>,
    Path(id): Path<String>,
) -> Response {
    let Some(registry) = &state.model_cards else {
        return Redirect303("/compliance/models".into()).into_response();
    };
    let _ = registry.delete(&id).await;
    if let Some(audit) = &state.audit {
        let _ = audit
            .append(
                session.username.clone(),
                computeza_audit::Action::UserAction,
                Some(format!("model-card/{id}")),
                serde_json::json!({
                    "action": "delete_model_card",
                    "id": id,
                }),
            )
            .await;
    }
    Redirect303("/compliance/models".into()).into_response()
}

/// GET /admin/pq-status -- read-only render of the binary's
/// post-quantum posture. Tracks two surfaces independently:
///
/// 1. TLS handshakes via [`computeza_channel_partner::pq::tls_readiness`].
///    We ship rustls + aws-lc-rs which offers X25519MLKEM768 by
///    default -- harvest-now-decrypt-later resistance is on as soon
///    as the binary boots.
///
/// 2. License envelopes: the active license is either classical-only
///    (Ed25519) or dual-signed (Ed25519 + ML-DSA). v0.0.x carries
///    the dual-sig shape; v0.1 wires the actual cryptographic
///    verification.
async fn admin_pq_status_handler(State(state): State<AppState>) -> Html<String> {
    let l = Localizer::english();
    let pq = computeza_channel_partner::pq::tls_readiness();
    let license = state.license.read().await.clone();
    Html(render_admin_pq_status(&l, &pq, license.as_ref()))
}

/// GET /api/license/status -- JSON status snapshot used by the banner
/// JS injected on every shell. Returns the discriminated status plus
/// a one-line message suitable for display.
async fn api_license_status_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let status = state.license_status().await;
    let (kind, message, severity) = match status {
        computeza_license::LicenseStatus::Active { days_remaining } if days_remaining <= 30 => (
            "expiring-soon",
            format!("License expires in {days_remaining} day(s) -- renew soon."),
            "warn",
        ),
        computeza_license::LicenseStatus::Active { .. } => ("active", String::new(), "ok"),
        computeza_license::LicenseStatus::None => ("none", String::new(), "ok"),
        computeza_license::LicenseStatus::Expired { days_since_expiry } => (
            "expired",
            format!(
                "License expired {days_since_expiry} day(s) ago -- the console is read-only until a fresh envelope is activated."
            ),
            "error",
        ),
        computeza_license::LicenseStatus::NotYetValid { days_until_valid } => (
            "not-yet-valid",
            format!(
                "License becomes effective in {days_until_valid} day(s) -- the console is read-only until then."
            ),
            "warn",
        ),
        computeza_license::LicenseStatus::Invalid(reason) => (
            "invalid",
            format!(
                "License envelope failed verification ({reason}) -- falling back to Community mode."
            ),
            "error",
        ),
    };
    Json(serde_json::json!({
        "kind": kind,
        "message": message,
        "severity": severity,
    }))
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
    /// Public install guide (prereqs + step-by-step setup).
    InstallGuide,
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
    /// Admin tenants page (multi-tenancy boundary management).
    Tenants,
    /// Operator studio (catalog browser + SQL editor).
    Studio,
    /// Admin branding / white-labeling page.
    Branding,
    /// Admin license envelope viewer.
    License,
    /// EU AI Act compliance evidence (model card registry +
    /// Article checklist).
    Compliance,
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
    let nav_studio = localizer.t("ui-nav-studio");
    let nav_install = localizer.t("ui-nav-install");
    let nav_status = localizer.t("ui-nav-status");
    let nav_state = localizer.t("ui-nav-state");
    let nav_secrets = localizer.t("ui-admin-secrets");
    let nav_account = localizer.t("ui-nav-account");
    let nav_audit = localizer.t("ui-audit-nav");
    let nav_admin_operators = localizer.t("ui-nav-admin-operators");
    let nav_admin_groups = localizer.t("ui-nav-admin-groups");
    let nav_admin_tenants = localizer.t("ui-nav-admin-tenants");
    let _ = localizer.t("ui-nav-admin-branding"); // i18n key preserved; rail item removed
    let nav_admin_license = localizer.t("ui-nav-admin-license");
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
<!-- Font Awesome 6 Free (solid + regular). Pinned to a specific
     SRI-able CDN release so a silent upstream change can't break
     the chrome. Operators on isolated networks can self-host the
     same release under /static/ and swap this href; the icon
     classes (fa-solid fa-...) stay identical. -->
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fortawesome/fontawesome-free@6.5.2/css/all.min.css" />
</head>
<body>
<div class="cz-topbar">
  <a href="/" class="cz-brand" title="{app_title}">
    <img src="/static/brand/computeza-logo.svg" alt="" />
    <span class="cz-brand-text">{app_title}</span>
  </a>
  <a href="/account" class="cz-topbar-account {nacc}">{nav_account}</a>
</div>
<nav class="cz-sidenav" aria-label="Primary navigation">
  <button type="button" class="cz-sidenav-toggle" id="cz-sidenav-toggle" aria-label="Toggle navigation rail"></button>
  <div class="cz-sidenav-section">
    <div class="cz-sidenav-section-header">Build</div>
    <a href="/studio" class="cz-sidenav-item {nwks}" data-label="{nav_studio}"><i class="fa-solid fa-flask fa-fw"></i><span class="cz-sidenav-label">{nav_studio}</span></a>
    <a href="/install" class="cz-sidenav-item {ni}" data-label="{nav_install}"><i class="fa-solid fa-cloud-arrow-down fa-fw"></i><span class="cz-sidenav-label">{nav_install}</span></a>
    <a href="/components" class="cz-sidenav-item {nc}" data-label="{nav_components}"><i class="fa-solid fa-cubes fa-fw"></i><span class="cz-sidenav-label">{nav_components}</span></a>
    <a href="/install-guide" class="cz-sidenav-item {nig}" data-label="Install Guide"><i class="fa-solid fa-book-open fa-fw"></i><span class="cz-sidenav-label">Install Guide</span></a>
  </div>
  <div class="cz-sidenav-sep"></div>
  <div class="cz-sidenav-section">
    <div class="cz-sidenav-section-header">Operate</div>
    <a href="/status" class="cz-sidenav-item {ns}" data-label="{nav_status}"><i class="fa-solid fa-heart-pulse fa-fw"></i><span class="cz-sidenav-label">{nav_status}</span></a>
    <a href="/state" class="cz-sidenav-item {nm}" data-label="{nav_state}"><i class="fa-solid fa-database fa-fw"></i><span class="cz-sidenav-label">{nav_state}</span></a>
    <a href="/audit" class="cz-sidenav-item {naud}" data-label="{nav_audit}"><i class="fa-solid fa-clipboard-list fa-fw"></i><span class="cz-sidenav-label">{nav_audit}</span></a>
    <a href="/compliance/eu-ai-act" class="cz-sidenav-item {ncmp}" data-label="Compliance"><i class="fa-solid fa-scale-balanced fa-fw"></i><span class="cz-sidenav-label">Compliance</span></a>
  </div>
  <div class="cz-sidenav-sep"></div>
  <div class="cz-sidenav-section">
    <div class="cz-sidenav-section-header">Admin</div>
    <a href="/admin/secrets" class="cz-sidenav-item {na}" data-label="{nav_secrets}"><i class="fa-solid fa-key fa-fw"></i><span class="cz-sidenav-label">{nav_secrets}</span></a>
    <a href="/admin/operators" class="cz-sidenav-item {nops}" data-label="{nav_admin_operators}"><i class="fa-solid fa-user-tie fa-fw"></i><span class="cz-sidenav-label">{nav_admin_operators}</span></a>
    <a href="/admin/groups" class="cz-sidenav-item {ngrp}" data-label="{nav_admin_groups}"><i class="fa-solid fa-users fa-fw"></i><span class="cz-sidenav-label">{nav_admin_groups}</span></a>
    <a href="/admin/tenants" class="cz-sidenav-item {nwsp}" data-label="{nav_admin_tenants}"><i class="fa-solid fa-building fa-fw"></i><span class="cz-sidenav-label">{nav_admin_tenants}</span></a>
    <a href="/admin/license" class="cz-sidenav-item {nlic}" data-label="{nav_admin_license}"><i class="fa-solid fa-certificate fa-fw"></i><span class="cz-sidenav-label">{nav_admin_license}</span></a>
  </div>
  <div class="cz-sidenav-spacer"></div>
</nav>
<div id="cz-license-banner-mount"></div>
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

// Side-rail toggle. Persists open/collapsed state in localStorage
// so a refresh / navigation doesn't reset the operator's choice.
// Applies the class BEFORE the body paints to avoid a layout flash
// (the inline <script> runs synchronously below the body element).
(function () {{
  var KEY = "cz-sidenav-open";
  try {{
    if (localStorage.getItem(KEY) === "1") {{
      document.body.classList.add("cz-sidenav-open");
    }}
  }} catch (_) {{}}
  var btn = document.getElementById("cz-sidenav-toggle");
  if (!btn) return;
  btn.addEventListener("click", function () {{
    var on = document.body.classList.toggle("cz-sidenav-open");
    try {{ localStorage.setItem(KEY, on ? "1" : "0"); }} catch (_) {{}}
  }});
}})();

// Timestamp display preference. Any element with data-ts-utc="<ISO8601>"
// gets its text rewritten to the operator's chosen format (UTC default
// or browser-local time). The /account page lets the operator toggle
// between the two; the choice is stored in localStorage under
// cz-tz-mode = "utc" | "local". Renderers opt in by emitting
// data-ts-utc="..." on the element that displays the timestamp.
window.czRewriteTimestamps = function () {{
  var mode = "utc";
  try {{ mode = localStorage.getItem("cz-tz-mode") || "utc"; }} catch (_) {{}}
  var nodes = document.querySelectorAll("[data-ts-utc]");
  for (var i = 0; i < nodes.length; i++) {{
    var n = nodes[i];
    var iso = n.getAttribute("data-ts-utc");
    if (!iso) continue;
    var d = new Date(iso);
    if (isNaN(d.getTime())) continue;
    if (mode === "local") {{
      // Format: 2026-05-15 12:34:56 (local TZ offset implied).
      var pad = function (n) {{ return n < 10 ? "0" + n : "" + n; }};
      n.textContent = d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate())
        + " " + pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
      n.title = iso + " (UTC)";
    }} else {{
      n.textContent = iso;
      n.title = "";
    }}
  }}
}};
window.czRewriteTimestamps();

// Password-field show/hide toggle. For every <input type="password">
// rendered on the page, we wrap the input in a relative-positioned
// span and inject a small clickable "eye" button. Clicking flips the
// input's `type` between "password" and "text" so the operator can
// confirm what they typed before submitting. This applies on /login,
// /setup, /admin/operators, /admin/secrets rotate, etc. -- every
// password field, with no per-page wiring.
//
// Why JS rather than CSS-only: there is no CSS selector that
// targets "the input is type=password vs text"; we must toggle the
// attribute. The button is type="button" so it does not trigger
// form submission, and the autocomplete attribute on the input is
// untouched so password managers keep working.
(function () {{
  var SHOW = "M1 12c2.5-5 7-8 11-8s8.5 3 11 8c-2.5 5-7 8-11 8s-8.5-3-11-8z M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z";
  var HIDE = "M3 3l18 18 M10.6 6.1A11 11 0 0 1 12 6c4 0 8.5 3 11 8a13 13 0 0 1-3.2 4.4 M6.7 6.7A13 13 0 0 0 1 12c2.5 5 7 8 11 8 1.5 0 2.9-.3 4.2-.9";
  function eyeSvg(showing) {{
    var d = showing ? HIDE : SHOW;
    return '<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="' + d + '"/></svg>';
  }}
  function decorate(input) {{
    if (input.dataset.czEyeBound === "1") return;
    input.dataset.czEyeBound = "1";
    var wrap = document.createElement("span");
    wrap.style.position = "relative";
    wrap.style.display = "inline-block";
    wrap.style.width = "100%";
    input.parentNode.insertBefore(wrap, input);
    wrap.appendChild(input);
    input.style.paddingRight = "2.4rem";
    var btn = document.createElement("button");
    btn.type = "button";
    btn.setAttribute("aria-label", "Show password");
    btn.setAttribute("title", "Show password");
    btn.style.position = "absolute";
    btn.style.right = "0.5rem";
    btn.style.top = "50%";
    btn.style.transform = "translateY(-50%)";
    btn.style.background = "transparent";
    btn.style.border = "0";
    btn.style.cursor = "pointer";
    btn.style.color = "inherit";
    btn.style.padding = "0.2rem";
    btn.style.lineHeight = "0";
    btn.innerHTML = eyeSvg(false);
    btn.addEventListener("click", function () {{
      var showing = input.type === "text";
      input.type = showing ? "password" : "text";
      btn.innerHTML = eyeSvg(!showing);
      btn.setAttribute("aria-label", showing ? "Show password" : "Hide password");
      btn.setAttribute("title", showing ? "Show password" : "Hide password");
    }});
    wrap.appendChild(btn);
  }}
  document.querySelectorAll('input[type="password"]').forEach(decorate);
}})();

// License-status banner. Fetches /api/license/status (a public
// endpoint) and injects a renewal banner above the page container
// when the active envelope is expiring, expired, invalid, or
// not-yet-valid. No-ops for the active or community-mode states.
(function () {{
  fetch("/api/license/status", {{ headers: {{ "accept": "application/json" }} }})
    .then(function (r) {{ return r.ok ? r.json() : null; }})
    .then(function (s) {{
      if (!s || s.severity === "ok" || !s.message) return;
      var mount = document.getElementById("cz-license-banner-mount");
      if (!mount) return;
      var color = s.severity === "error"
        ? "rgba(255, 157, 166, 0.55)"
        : "rgba(255, 196, 87, 0.55)";
      var fg = s.severity === "error" ? "var(--fail)" : "inherit";
      mount.innerHTML =
        '<div role="status" style="border:1px solid ' + color +
        '; padding: 0.8rem 1.1rem; margin: 0.6rem 1.5rem 0; border-radius: 0.5rem; background: var(--surface-1, transparent); color: ' + fg + ';"><strong>License:</strong> ' +
        s.message.replace(/</g, "&lt;") +
        ' <a href="/admin/license" style="margin-left: 0.6rem;">Manage</a></div>';
    }})
    .catch(function () {{ /* swallow -- banner is best-effort */ }});
}})();
</script>
</body>
</html>"#,
        nc = nav_class(NavLink::Components),
        nig = nav_class(NavLink::InstallGuide),
        ni = nav_class(NavLink::Install),
        nwks = nav_class(NavLink::Studio),
        ns = nav_class(NavLink::Status),
        nm = nav_class(NavLink::State),
        na = nav_class(NavLink::Secrets),
        naud = nav_class(NavLink::Audit),
        nops = nav_class(NavLink::Operators),
        ngrp = nav_class(NavLink::Groups),
        nwsp = nav_class(NavLink::Tenants),
        nlic = nav_class(NavLink::License),
        ncmp = nav_class(NavLink::Compliance),
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

    // Stack at a glance -- auto-rendered from the COMPONENTS array
    // so adding a managed component (e.g. Sail) makes the homepage
    // catch up without copy edits. Each card lights up the icon
    // glyph + name + role line + a link to /components for the
    // full upstream metadata (license, version, source).
    let stack_eyebrow = localizer.t("ui-landing-stack-eyebrow");
    let stack_title = localizer
        .t("ui-landing-stack-title")
        .replace("COUNT_PLACEHOLDER", &format!("{}", COMPONENTS.len()));
    let stack_subtitle = localizer.t("ui-landing-stack-subtitle");
    let stack_cards: String = COMPONENTS
        .iter()
        .map(|c| {
            let name = localizer.t(c.name_key);
            let role = localizer.t(c.role_key);
            // First letter as the icon glyph, mono-styled so the
            // grid stays uniform regardless of component name length.
            let glyph: String = name
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "·".into());
            format!(
                r#"<a class="cz-feature cz-stack-card" href="/components#{slug}">
<span class="cz-feature-icon">{glyph}</span>
<h3 class="cz-feature-title">{name}</h3>
<p class="cz-feature-body">{role}</p>
</a>"#,
                slug = html_escape(c.slug),
                glyph = html_escape(&glyph),
                name = html_escape(&name),
                role = html_escape(&role),
            )
        })
        .collect();

    // Trust + compliance pillars
    let trust_eyebrow = localizer.t("ui-landing-trust-eyebrow");
    let trust_title = localizer.t("ui-landing-trust-title");
    let trust_subtitle = localizer.t("ui-landing-trust-subtitle");
    let trust_pillars = [
        (
            localizer.t("ui-landing-trust-1-title"),
            localizer.t("ui-landing-trust-1-body"),
            "/admin/license",
            "License envelope",
        ),
        (
            localizer.t("ui-landing-trust-2-title"),
            localizer.t("ui-landing-trust-2-body"),
            "/admin/secrets",
            "Secrets setup",
        ),
        (
            localizer.t("ui-landing-trust-3-title"),
            localizer.t("ui-landing-trust-3-body"),
            "/admin/pq-status",
            "PQ readiness",
        ),
        (
            localizer.t("ui-landing-trust-4-title"),
            localizer.t("ui-landing-trust-4-body"),
            "/compliance/eu-ai-act",
            "EU AI Act",
        ),
    ];
    let trust_cards: String = trust_pillars
        .iter()
        .map(|(t, b, href, link)| {
            format!(
                r#"<div class="cz-feature">
<h3 class="cz-feature-title">{t}</h3>
<p class="cz-feature-body">{b}</p>
<p style="margin: 0.6rem 0 0; font-size: 0.85rem;"><a href="{href}">{link} -&gt;</a></p>
</div>"#,
                t = html_escape(t),
                b = html_escape(b),
                href = html_escape(href),
                link = html_escape(link),
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

    // Two-tier pricing: Standard (seat-capped per-seat) and
    // Enterprise (custom). No free / Community tier -- Computeza
    // is paid-only commercial software.
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
            localizer.t("ui-landing-pricing-1-feature-6"),
        ],
        &localizer.t("ui-landing-pricing-1-cta"),
        "mailto:hello@computeza.eu?subject=Computeza%20Standard%20-%20seat%20pricing",
        Some(&localizer.t("ui-landing-pricing-1-badge")),
        true,
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
            localizer.t("ui-landing-pricing-2-feature-6"),
        ],
        &localizer.t("ui-landing-pricing-2-cta"),
        "mailto:hello@computeza.eu?subject=Computeza%20Enterprise%20-%20contract%20pricing",
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
<a class="cz-btn cz-btn-lg" href="/install-guide">Install guide</a>
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
<p class="cz-landing-section-eyebrow">{stack_eyebrow}</p>
<h2 class="cz-landing-section-title">{stack_title}</h2>
<p class="cz-landing-section-subtitle">{stack_subtitle}</p>
</div>
<div class="cz-stack-grid">{stack_cards}</div>
</section>

<section class="cz-landing-section">
<div class="cz-landing-section-head">
<p class="cz-landing-section-eyebrow">{trust_eyebrow}</p>
<h2 class="cz-landing-section-title">{trust_title}</h2>
<p class="cz-landing-section-subtitle">{trust_subtitle}</p>
</div>
<div class="cz-feature-grid">{trust_cards}</div>
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
<div class="cz-pricing-grid cz-pricing-grid-2col">
{tier1}
{tier2}
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
        stack_eyebrow = html_escape(&stack_eyebrow),
        stack_title = html_escape(&stack_title),
        stack_subtitle = html_escape(&stack_subtitle),
        stack_cards = stack_cards,
        trust_eyebrow = html_escape(&trust_eyebrow),
        trust_title = html_escape(&trust_title),
        trust_subtitle = html_escape(&trust_subtitle),
        trust_cards = trust_cards,
        audiences_eyebrow = html_escape(&audiences_eyebrow),
        audiences_title = html_escape(&audiences_title),
        audiences_subtitle = html_escape(&audiences_subtitle),
        persona_cards = persona_cards,
        pricing_eyebrow = html_escape(&pricing_eyebrow),
        pricing_title = html_escape(&pricing_title),
        pricing_subtitle = html_escape(&pricing_subtitle),
        tier1 = tier1,
        tier2 = tier2,
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
        ("trino", "sql-engine", "Apache-2.0", "ok"),
        ("sail", "spark-engine", "Apache-2.0", "ok"),
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

/// Render the public `/install-guide` page -- the operator-facing
/// step-by-step setup reference. Linked from the top nav + landing
/// CTAs.
///
/// Content scope:
/// 1. Prerequisites table (OS, hardware, network, ports, user).
/// 2. Step 1: build (from source for v0.0.x; signed releases land
///    in v0.1+).
/// 3. Step 2: first run + secrets passphrase.
/// 4. Step 3: install components from `/install`.
/// 5. Step 4: optional license activation.
/// 6. WSL2-specific notes (systemd toggle, port forwarding, ext4-
///    vs-NTFS guidance).
/// 7. Production hardening checklist.
/// 8. Troubleshooting common errors.
/// 9. Where to get help.
///
/// The content is largely inlined rather than fully tokenised
/// through the i18n bundle so the page reads coherently to a human
/// drafting it; the high-frequency strings (section headers, code
/// fences) can move into ui.ftl in a follow-up without touching the
/// page structure.
#[must_use]
pub fn render_install_guide(localizer: &Localizer) -> String {
    let title = "Install guide";

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>Step-by-step setup for the Computeza operator console and the managed-component data plane. v0.0.x ships a single binary that installs, configures, and supervises every component natively on the host operating system. The list below is the canonical path from a fresh machine to a running stack.</p>
<p class="cz-muted">Estimated time: <strong>10-20 minutes</strong> on a typical workstation (Rust toolchain warm + 1 Gbps network); add 5-10 minutes per managed component the first time it pulls binaries.</p>
</section>

<section class="cz-section" id="prereqs">
<h2>Prerequisites</h2>
<p class="cz-muted">v0.0.x targets <strong>Ubuntu Linux x86_64 only</strong> for the full 11-component data plane. The constraint is Databend, which ships glibc-linked binaries we've verified only against Ubuntu's glibc + systemd userspace; other distros (Debian, Fedora, RHEL, openSUSE, Arch) will install most components but Databend's startup may fail with a glibc-version mismatch or systemd-unit semantic difference. macOS and Windows ship a Postgres + Kanidm subset and gain the remaining components in v0.1+. Verify each row before proceeding.</p>
<div class="cz-table-wrap">
<table class="cz-table">
<thead><tr><th>Requirement</th><th>Minimum</th><th>Recommended</th><th>Notes</th></tr></thead>
<tbody>
<tr><td class="cz-strong">Operating system</td><td>Ubuntu 22.04 LTS</td><td>Ubuntu 24.04 LTS</td><td><strong>Ubuntu-only in v0.0.x</strong>; Databend's binary distribution is the binding constraint. Other systemd-based distros likely work for the other 10 components but are unverified end-to-end. macOS 13+ and Windows 11 supported for a partial stack (Postgres + Kanidm) -- see <a href="/components">/components</a>. v0.1+ broadens the matrix to Debian / Fedora / RHEL after Databend release-engineering catches up.</td></tr>
<tr><td class="cz-strong">Architecture</td><td>x86_64</td><td>x86_64</td><td>ARM64 lands in v0.1+ (spec section 10).</td></tr>
<tr><td class="cz-strong">CPU</td><td>2 vCPU</td><td>4 vCPU</td><td>Postgres + Restate + Greptime are the heaviest co-residents.</td></tr>
<tr><td class="cz-strong">RAM</td><td>4 GiB</td><td>8 GiB+</td><td>Per-component RAM dominated by Postgres shared_buffers + Greptime + Databend caches.</td></tr>
<tr><td class="cz-strong">Disk</td><td>20 GiB free under /var</td><td>100 GiB SSD</td><td>Object storage (Garage) and Iceberg data dirs dominate footprint. Mount /var/lib/computeza on a fast disk.</td></tr>
<tr><td class="cz-strong">User account</td><td>Operator with <code>sudo</code></td><td>Same</td><td>The install path writes /var/lib/computeza, /etc/systemd/system, and /usr/local/bin. The wrapping binary re-execs itself with sudo when needed.</td></tr>
<tr><td class="cz-strong">systemd</td><td>active</td><td>active</td><td><code>systemctl is-system-running</code> must report <code>running</code> or <code>degraded</code>. WSL2 users: enable via <code>/etc/wsl.conf</code> (see WSL section below).</td></tr>
<tr><td class="cz-strong">Network egress</td><td>HTTPS to <code>get.enterprisedb.com</code>, <code>github.com</code>, upstream component sites</td><td>Same + local mirror</td><td>The drivers download binaries on first install. Air-gapped operators can pre-stage tarballs under <code>&lt;root_dir&gt;/binaries/&lt;version&gt;/</code> and the install detects them.</td></tr>
<tr><td class="cz-strong">Inbound ports</td><td>8400 (console)</td><td>8400 + per-component</td><td>Component defaults: Postgres 5432, Garage 3900, OpenFGA 8080, Qdrant 6333, Restate 9070, GreptimeDB 4000, Grafana 3000, Lakekeeper 8181, Kanidm 8443, Databend 8000, XTable 8090. Bind 127.0.0.1 for local-only.</td></tr>
<tr><td class="cz-strong">Rust toolchain</td><td>1.83+</td><td>Latest stable</td><td>Required only to build from source. Pre-built signed releases ship in v0.1.</td></tr>
</tbody>
</table>
</div>
</section>

<section class="cz-section" id="step-1-build">
<h2>Step 1 -- get the binary</h2>
<p>v0.0.x is build-from-source. Signed pre-built releases land in v0.1+ (spec section 13).</p>
<ol class="cz-ol">
<li>Install the build toolchain:
<pre><code>sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"</code></pre>
</li>
<li>Clone the repository:
<pre><code>cd ~
git clone https://github.com/indritkalaj/computeza
cd computeza</code></pre>
</li>
<li>Build the binary in release mode (the dev profile works for kicking tires but boots slower):
<pre><code>cargo build --release --bin computeza
./target/release/computeza --version</code></pre>
</li>
</ol>
<p class="cz-muted">First build takes 5-10 minutes on a modern laptop. Subsequent builds reuse the cache and finish in seconds.</p>
</section>

<section class="cz-section" id="step-2-first-run">
<h2>Step 2 -- first run + the secrets passphrase</h2>
<p>The operator console encrypts every generated credential under a single passphrase. <strong>Computeza auto-generates this passphrase on first boot</strong> -- you do not need to run <code>openssl rand</code> or set any environment variable yourself. The binary writes a 256-bit CSPRNG passphrase to <code>&lt;state_db_parent&gt;/computeza-passphrase</code> (mode 0600) and reuses it on every subsequent start.</p>
<ol class="cz-ol">
<li>Boot the console under sudo. The install path writes <code>/var/lib/computeza</code> + <code>/etc/systemd/system</code> + <code>/usr/local/bin</code> so it needs root; the <code>-E</code> flag preserves environment variables across the sudo boundary (matters when you later integrate with Vault / KMS via <code>COMPUTEZA_SECRETS_PASSPHRASE</code>).
<pre><code>sudo -E ./target/release/computeza serve --addr 127.0.0.1:8400 \
    --state-db /var/lib/computeza/computeza-state.db</code></pre>
</li>
<li>On first run the binary logs a prominent warning:
<pre><code>WARN: Generated a NEW secrets passphrase. To keep stored
      secrets recoverable across hardware migrations and
      disaster recovery you MUST back up THREE things together:
      (1) /var/lib/computeza/computeza-passphrase
      (2) /var/lib/computeza/computeza-secrets.salt
      (3) /var/lib/computeza/computeza-secrets.jsonl</code></pre>
Back those three files up together. Losing any one renders every stored secret permanently unrecoverable -- by design.
</li>
<li>Open <a href="/">http://localhost:8400</a> in a browser. The first request lands on <code>/setup</code> where you mint the initial admin account -- the only unauthenticated form on the console after first boot.</li>
<li><strong>Production: bring your own key.</strong> Operators integrating with HashiCorp Vault, AWS KMS, GCP KMS, PKCS#11, or TPM set <code>COMPUTEZA_SECRETS_PASSPHRASE</code> in the systemd EnvironmentFile= and the auto-generated file is ignored entirely. v0.1 ships a <code>KeyProvider</code> trait so the binary asks Vault for the key directly without staging anything on disk.</li>
</ol>
<p class="cz-muted">The "no manual openssl step" change landed on 2026-05-13. Older operator-side docs may still tell you to run <code>openssl rand -hex 32</code> -- ignore those; the binary does it for you.</p>
</section>

<section class="cz-section" id="step-3-components">
<h2>Step 3 -- install managed components</h2>
<p>Once signed in, the <a href="/install">/install</a> page is the one-screen hub: every managed component is listed as an accordion row with a per-component form for port + data-directory overrides. Each driver:</p>
<ul>
<li>Downloads its binaries from the upstream (GitHub Releases for most; <code>dl.grafana.com</code> for Grafana; <code>git.deuxfleurs.fr</code> source tarball for Garage; <code>github.com/kanidm/kanidm</code> source tarball for Kanidm).</li>
<li>Drops a data directory under <code>/var/lib/computeza/&lt;component&gt;/</code> (or your override).</li>
<li>Writes a systemd unit named <code>computeza-&lt;component&gt;.service</code> with <code>WorkingDirectory</code> + <code>RuntimeDirectory</code> + a hardened sandbox (<code>ProtectSystem=strict</code>, <code>NoNewPrivileges</code>, etc.).</li>
<li>For components with cross-component dependencies (Lakekeeper -&gt; PostgreSQL), runs the provisioning step (creates the role + database via <code>sudo -u postgres psql</code>) before the unit starts.</li>
<li>For source-built components (Kanidm, Garage), uses the <strong>release-swap</strong> pattern -- atomic-swaps <code>&lt;root&gt;/current</code> to point at a fresh <code>releases/&lt;timestamp&gt;-v&lt;version&gt;/</code> directory, with a pre-flight <code>--help</code> probe before the swap. Rollback is one symlink command.</li>
<li><code>systemctl stop</code> before <code>enable --now</code> so re-installs actually pick up the rewritten unit (otherwise the in-memory unit of the running daemon keeps using the OLD env vars).</li>
<li>Waits for the TCP port. On timeout, the install-result page surfaces the last 60 lines of the unit's journal inline -- no <code>journalctl</code> round-trip needed to diagnose.</li>
</ul>
<p><strong>Install order matters.</strong> Postgres should be installed first because Lakekeeper (and v0.1+ identity-store) consume it. The unified "Install everything" button handles ordering automatically; if you install per-component, do Postgres -&gt; OpenFGA -&gt; Kanidm -&gt; ... -&gt; Lakekeeper.</p>
<p class="cz-muted">Re-running an install for an already-installed component is idempotent: data directories are preserved, config files marked <code>overwrite_if_present: false</code> (Databend, Garage) keep operator edits across re-installs, source builds atomic-swap rather than overwrite in place, and the postgres provisioning SQL is wrapped in <code>DO $$ IF NOT EXISTS $$</code> blocks.</p>
</section>

<section class="cz-section" id="step-4-license">
<h2>Step 4 -- activate a license (optional)</h2>
<p>Computeza runs in <strong>Community mode</strong> without a license attached -- all 11 components install and run; the operator console is fully functional. License activation is what turns on tier-gated features (seat caps, expiry kill-switch, channel-partner gRPC API) and shows the entitlement chain to your reseller.</p>
<ol class="cz-ol">
<li>Get a license envelope from your reseller (or from <a href="mailto:hello@computeza.eu">hello@computeza.eu</a> for direct deals).</li>
<li>In the console, navigate to <a href="/admin/license">/admin/license</a> and paste the envelope JSON into the <em>Activate</em> form.</li>
<li>The binary verifies the Ed25519 signature against the baked-in trusted-root key + checks the validity window + persists the envelope at <code>&lt;state_db_parent&gt;/license.json</code>. A success page lists tier, seats, and the resale chain.</li>
</ol>
<p class="cz-muted">License files are tamper-evident: any edit to the JSON breaks the signature and the binary refuses to load it on the next restart. The dual-signature PQ envelope (Ed25519 + ML-DSA) ships in v0.1; envelopes signed only with Ed25519 stay valid through the transition.</p>
</section>

<section class="cz-section" id="wsl">
<h2>WSL2 specifics (Windows 11)</h2>
<p>WSL2 is a fully supported development target. Two settings make the difference between "kicks tires" and "behaves like bare-metal Linux":</p>
<ol class="cz-ol">
<li><strong>Enable systemd.</strong> WSL2 ships with systemd support but it's off by default. Inside the Ubuntu shell:
<pre><code>sudo tee /etc/wsl.conf &gt;/dev/null &lt;&lt;'EOF'
[boot]
systemd=true
EOF</code></pre>
Then from Windows CMD / PowerShell: <code>wsl --shutdown</code>, re-open Ubuntu, verify with <code>systemctl is-system-running</code>.
</li>
<li><strong>Build inside the Linux filesystem, not <code>/mnt/c/...</code>.</strong> Cargo + rust-analyzer perform 5-10x faster on ext4 than on the SMB bridge into NTFS. Clone the repo under <code>~/computeza</code>, not <code>/mnt/c/Users/&lt;you&gt;/computeza</code>.</li>
<li><strong>Localhost forwarding.</strong> WSL2 auto-forwards <code>127.0.0.1</code> bindings to the Windows host, so the console at <code>localhost:8400</code> reaches Edge / Chrome on Windows without any port mapping.</li>
<li><strong>VS Code WSL extension.</strong> Install via <code>code --install-extension ms-vscode-remote.remote-wsl</code> in CMD / PowerShell. Once installed, run <code>code .</code> from your WSL shell inside <code>~/computeza</code> -- VS Code keeps its UI on Windows but runs rust-analyzer, the terminal, and the debugger inside Linux.</li>
</ol>
</section>

<section class="cz-section" id="hardening">
<h2>Production hardening checklist</h2>
<p>For an internet-reachable deployment, work through this list before exposing the console publicly. Each item maps onto a specific spec section (referenced in the in-repo AGENTS.md audit trail).</p>
<ul>
<li><strong>Run <code>computeza serve</code> as a systemd unit.</strong> The install wizard at <a href="/install">/install</a> ships a <em>Self-install</em> button (v0.0.x landing) that drops a unit at <code>/etc/systemd/system/computeza.service</code> with hardened defaults (<code>NoNewPrivileges</code>, <code>ProtectSystem=strict</code>, <code>PrivateTmp</code>, <code>ReadWritePaths=/var/lib/computeza</code>).</li>
<li><strong>Put a reverse proxy + TLS cert in front.</strong> The console binds plain HTTP on 127.0.0.1; the canonical pattern is Caddy / nginx / haproxy fronting it on 443 with Let's Encrypt or your enterprise CA. v0.1 ships an in-tree TLS terminator with a hybrid X25519+ML-KEM cipher for PQ readiness.</li>
<li><strong>Move the secrets passphrase out of the environment.</strong> v0.0.x reads <code>COMPUTEZA_SECRETS_PASSPHRASE</code> from the environment. v0.1 plugs in a <code>KeyProvider</code> trait with implementations for HashiCorp Vault, AWS KMS, GCP KMS, PKCS#11, and TPM -- the binary asks the provider for the master key at boot instead of deriving from a passphrase.</li>
<li><strong>Bind the console to a non-routed interface.</strong> Even with a reverse proxy, keep <code>--addr 127.0.0.1:8400</code> and let the proxy be the only thing on a routable port. Defense-in-depth.</li>
<li><strong>Configure RBAC.</strong> The default operator account is in the <code>admins</code> group. Create per-team accounts under <a href="/admin/operators">/admin/operators</a> with narrower group memberships (<code>operators</code> can install but not manage other operators; <code>viewers</code> have read-only access to <a href="/status">/status</a> + <a href="/audit">/audit</a>).</li>
<li><strong>Pin license + audit-log keys offline.</strong> Back up <code>&lt;state_db_parent&gt;/audit.key</code> and the operator account file (<code>operators.jsonl</code>) alongside the secrets bundle. Without the audit key, the signed log cannot be re-verified by an external auditor.</li>
<li><strong>Subscribe to security advisories.</strong> Watch <a href="https://github.com/indritkalaj/computeza/security/advisories">github.com/indritkalaj/computeza/security/advisories</a>. CVEs in the managed-component upstreams (Postgres, Restate, etc.) flow into Computeza's release notes within 48 hours.</li>
</ul>
</section>

<section class="cz-section" id="troubleshooting">
<h2>Troubleshooting</h2>
<p class="cz-muted"><strong>Read the install-result page first.</strong> When a component fails, the result page splices the last 60 lines of <code>journalctl -u computeza-&lt;component&gt;</code> directly into the error message. The actual daemon-side cause is inline -- no <code>journalctl</code> round-trip required for most failures.</p>

<dl class="cz-dl">
<dt>"Permission denied (os error 13)" during a component install</dt>
<dd>The install path writes <code>/var/lib/computeza/*</code> and <code>/etc/systemd/system/*</code> -- both require root. Re-run with <code>sudo -E ./target/release/computeza serve ...</code>. The <code>-E</code> preserves environment variables (passphrase overrides, proxy settings, etc.) across the sudo boundary.</dd>

<dt>"Text file busy (os error 26)" on a source-built component re-install</dt>
<dd>Linux refuses to overwrite an executable that's currently mmap'd by a running process. The driver mitigates by <code>systemctl stop</code> + atomic rename, plus the release-swap pattern for kanidm + garage (binary lives under <code>&lt;root&gt;/releases/&lt;id&gt;/</code> and <code>current</code> is a symlink that's atomic-swapped). If you ever see this error in v0.0.x, you have a stale binary running outside systemd's tracking -- kill it with <code>pkill &lt;binary&gt;</code> and re-install.</dd>

<dt>Service "did not become ready on port X within 30s"</dt>
<dd>The driver's wait-for-port probe timed out. The error message now carries the unit's journal tail (60 lines) -- read it. Common daemon-side causes: missing config / env var, port already in use, sandbox blocking a relative path (we set <code>WorkingDirectory=&lt;root&gt;</code> + <code>RuntimeDirectory=&lt;component&gt;</code> in every unit to avoid this), or an upstream-side regression in the daemon itself.</dd>

<dt>"Read-only file system" inside a managed daemon</dt>
<dd>The unit's <code>ProtectSystem=strict</code> sandbox makes everything outside <code>ReadWritePaths</code> read-only. The driver lists <code>ReadWritePaths=&lt;root&gt;</code> + sets <code>WorkingDirectory=&lt;root&gt;</code> so relative writes inside the daemon land in writable territory. If a daemon writes to an absolute path outside <code>&lt;root&gt;</code>, file a bug -- the driver should be passing that path as a config flag rather than letting the daemon hardcode it.</dd>

<dt>"postgres binaries not found"</dt>
<dd>The Linux Postgres driver searches <code>/usr/lib/postgresql/&lt;v&gt;/bin</code> (Ubuntu / Debian default) for majors 13-18. When no distro postgres is detected AND the install runs as root, the driver auto-invokes <code>apt-get install -y postgresql postgresql-contrib</code>. If the auto-install is what fails, the error tells you why -- typically not root, or no internet egress to <code>archive.ubuntu.com</code>. dnf / zypper / pacman fallbacks exist in the driver but only the apt path is verified end-to-end in v0.0.x.</dd>

<dt>"Failed to connect to bus" during systemd registration</dt>
<dd>systemd isn't running. On WSL2, enable it via <code>/etc/wsl.conf</code> and <code>wsl --shutdown</code> (see WSL section above). On a minimal LXC container, ensure the container template has systemd as PID 1. systemd-free hosts are not supported in v0.0.x; the v0.1 spec adds OpenRC + supervisord variants.</dd>

<dt>Kanidm install: "could not find `kanidmd` in registry"</dt>
<dd>You're on an older binary. The kanidm driver was switched on 2026-05-13 from <code>cargo install --version</code> (crates.io, often stale) to <code>cargo build --release --bin kanidmd</code> against the upstream git tag. <code>git pull &amp;&amp; cargo build</code> and re-install.</dd>

<dt>Kanidm install: "Can't find external/bootstrap.bundle.min.js"</dt>
<dd>The unit's <code>WorkingDirectory</code> wasn't set to the source tree where kanidm's web-UI assets live. Fixed on 2026-05-13: the driver now sets <code>WorkingDirectory=&lt;root&gt;/current/src/server/daemon</code> so kanidm's relative <code>../core/static/...</code> lookup resolves. Re-install if you're seeing this on an older binary.</dd>

<dt>Lakekeeper install: "A connection string or postgres host must be provided"</dt>
<dd>Lakekeeper needs a postgres connection AND a migrated schema. The driver provisions both: it creates a dedicated <code>lakekeeper</code> postgres role + database (idempotent <code>DO $$ IF NOT EXISTS $$</code> blocks), wires the connection details into the unit's <code>Environment=</code> lines, and runs <code>lakekeeper migrate</code> via <code>ExecStartPre=</code> before <code>serve</code>. If you still see this after a re-install with the latest binary, run <code>strings &lt;lakekeeper-binary&gt; | grep -E 'LAKEKEEPER|PG_DATABASE'</code> to dump the env-var names the binary actually parses (env-var naming has changed across lakekeeper releases) and open an issue with the output.</dd>

<dt>Postgres reconciler logs "password authentication failed for user postgres"</dt>
<dd>The reconciler is trying to verify postgres via TCP but can't resolve the spec's <code>superuser_password_ref</code> against the secrets store. This is benign if you're not relying on reconciler health -- the warning fires every 30s, the component still works. Resolve it by attaching the secrets store (auto-attached as of 2026-05-13 even without setting the env var) and re-installing postgres so the generated password is persisted encrypted.</dd>

<dt>License banner reads "Not yet valid" or "Signature failed"</dt>
<dd>The envelope's <code>not_before</code> is in the future, the chain anchor doesn't match the baked-in trusted-root key, or the JSON was edited after signing. Re-request a fresh envelope from your reseller; the binary refuses tampered envelopes by design.</dd>

<dt>Console boots but pages return 502 / connection refused from the reverse proxy</dt>
<dd>Check the bind address: if you set <code>--addr 0.0.0.0:8400</code> the reverse proxy reaches it on the public interface; for the recommended 127.0.0.1 binding, both the proxy and the console must be on the same host.</dd>

<dt>An installed component shows "Failed" on /status</dt>
<dd>Check the per-component systemd log: <code>journalctl -u computeza-&lt;component&gt; -n 200 --no-pager</code>. The most common cause is a port conflict with a pre-existing service on the same host (e.g. apt-installed Postgres on 5432 while the Computeza-managed Postgres tries to claim it). Disable the conflicting service with <code>sudo systemctl stop &lt;name&gt; &amp;&amp; sudo systemctl disable &lt;name&gt;</code>.</dd>

<dt>"Incorrect username or password" at /login</dt>
<dd>v0.0.x has no password-reset email flow. Recovery is to delete the operator file and re-mint the first admin via <code>/setup</code>:
<pre><code>rm /var/lib/computeza/operators.jsonl
# restart serve, then visit /setup
</code></pre>
If the file is at a different path, it's alongside <code>state.db</code>. v0.1 wires an external IdP (Entra ID / Okta / Auth0) via the <code>computeza-identity-federation</code> crate so password reset becomes an IdP concern.</dd>

<dt>Forensics: who did what, and when?</dt>
<dd>Every state change is appended to <code>&lt;state_db_parent&gt;/audit.jsonl</code> with an Ed25519 chained signature. View it under <a href="/audit">/audit</a>. The verifying key is at <code>audit.key</code>; back this up so an external auditor can verify the chain independently. Credential downloads via <code>/install/credentials.json</code> are recorded -- so a future breach investigation can answer "who pulled the install credentials, when".</dd>
</dl>
</section>

<section class="cz-section" id="help">
<h2>Where to get help</h2>
<ul>
<li><strong>Open an issue:</strong> <a href="https://github.com/indritkalaj/computeza/issues">github.com/indritkalaj/computeza/issues</a> -- bug reports, feature requests, and questions about the spec.</li>
<li><strong>Security disclosures:</strong> private vulnerability reports through GitHub's security advisories tab on the same repo.</li>
<li><strong>Commercial inquiries:</strong> <a href="mailto:hello@computeza.eu">hello@computeza.eu</a> for reseller / channel-partner agreements + enterprise tier negotiation.</li>
<li><strong>Component upstreams:</strong> bugs in a managed component itself (Postgres core, Restate runtime, etc.) belong upstream. See <a href="/components">/components</a> for project links + licenses.</li>
</ul>
</section>"#,
        title = html_escape(title),
    );

    render_shell(localizer, title, NavLink::InstallGuide, &body)
}

/// Minimal percent-encoder for a single URL path segment. Encodes
/// everything except unreserved chars + `-._~`. Not a full RFC 3986
/// implementation; built for resource kinds and names, which are
/// kebab-case ASCII per the studio conventions.
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
        // Linux install live: bundles Adoptium Temurin JRE 21 into
        // <root>/jre via ensure_bundled_temurin_jre, shells to Maven
        // (mvn on host) to resolve org.apache.xtable:xtable-utilities
        // + transitive deps from Maven Central into <root>/lib,
        // registers a systemd unit running `java -cp <root>/lib/* ...`.
        // Linux only for v0.0.x. Apache 2.0 licensed.
        available: true,
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
        slug: "trino",
        name_key: "component-trino-name",
        role_key: "component-trino-role",
        // Linux install live: downloads the trino-server tarball from
        // the Trino GitHub releases, bundles Temurin JRE 21, generates
        // etc/{node,config,jvm,log}.properties, registers a systemd
        // unit running `bin/launcher run`. Studio's SQL editor routes
        // all SQL queries here; Iceberg-REST catalogs are configured
        // at bootstrap time via etc/catalog/<warehouse>.properties.
        // Apache 2.0 licensed.
        available: true,
    },
    ComponentEntry {
        slug: "sail",
        name_key: "component-sail-name",
        role_key: "component-sail-role",
        // Linux install live: creates a Python venv under
        // /var/lib/computeza/sail/venv, `pip install`s pysail +
        // pyspark-client into it, registers a systemd unit running
        // `<venv>/bin/sail spark server --port 50051`. Spark Connect
        // (gRPC) endpoint. Studio routes Python/Spark queries here
        // and SQL queries to Databend -- both engines share the same
        // Lakekeeper Iceberg catalog. Apache 2.0 licensed.
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
    // Trino comes after lakekeeper so its bootstrap step can write
    // the Iceberg-REST catalog properties file against a running
    // Lakekeeper. Trino replaces Databend as Studio's default SQL
    // engine; Databend stays installable for operators who pick it
    // explicitly, but the auto-router on /studio/sql/execute routes
    // SQL to Trino when both are present.
    "trino",
    // Sail comes after the SQL engines so its bootstrap step can
    // find a running Iceberg-REST catalog when wire_sail_iceberg_catalog
    // fires. After Databend + Trino so all three engines spin up
    // roughly in parallel from the operator's perspective on the
    // install results page.
    "sail",
    // xtable goes last because the dataset config it eventually
    // reads (datasets.yaml under <root>) typically references
    // upstream sources living in other components (garage / lakekeeper).
    "xtable",
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
        "trino" => 8088,
        "sail" => 50051,
        "grafana" => 3000,
        "restate" => 8081,
        "xtable" => 8090,
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
fn render_unified_component_card(
    localizer: &Localizer,
    c: &ComponentEntry,
    is_installed: bool,
) -> String {
    let name = localizer.t(c.name_key);
    let role = localizer.t(c.role_key);
    let identity_label = localizer.t("ui-install-card-identity");
    let identity_help = localizer.t("ui-install-card-identity-help");
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

    // Badge precedence: Installed > Planned > Available. "Installed"
    // is the highest-signal status because it tells the operator the
    // component actually exists on this host (vs. just being a
    // shippable target). Computed from the metadata store before this
    // render is called -- see compute_installed_slugs().
    let installed_text = localizer.t("ui-install-status-installed");
    let (badge_class, badge_text) = if is_installed {
        ("cz-badge cz-badge-ok", installed_text.as_str())
    } else if c.available {
        ("cz-badge cz-badge-info", status_available.as_str())
    } else {
        ("cz-badge cz-badge-info", status_planned.as_str())
    };

    let disabled_attr = if c.available { "" } else { " disabled" };

    // Per-card action: Install (when missing) or Uninstall (when
    // installed). These hit the same per-component routes that have
    // existed for power users / CI scripts since v0.0.x landed, just
    // surfaced in the hub UI so operators can recover from a single
    // failed component without re-running the bulk install. The
    // bulk Install-All button at the bottom of the page is still the
    // happy path for fresh setups.
    let action_label_install = localizer.t("ui-install-card-action-install");
    let action_label_uninstall = localizer.t("ui-install-card-action-uninstall");
    let action_help_installed = localizer.t("ui-install-card-action-help-installed");
    let action_help_missing = localizer.t("ui-install-card-action-help-missing");
    let action_block = if !c.available {
        // Planned components have no per-component handler yet.
        String::new()
    } else if is_installed {
        format!(
            r#"<div style="margin-top: 0.85rem; padding: 0.75rem 0.9rem; border-radius: 0.5rem; background: rgba(255,99,99,0.08); border: 1px solid rgba(255,99,99,0.25);">
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.82rem;">{help}</p>
<a class="cz-btn cz-btn-danger" href="/install/{slug}/uninstall">{label}</a>
</div>"#,
            slug = slug,
            label = html_escape(&action_label_uninstall),
            help = html_escape(&action_help_installed),
        )
    } else {
        format!(
            r#"<div style="margin-top: 0.85rem; padding: 0.75rem 0.9rem; border-radius: 0.5rem; background: rgba(74, 158, 255, 0.08); border: 1px solid rgba(74, 158, 255, 0.25);">
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.82rem;">{help}</p>
<a class="cz-btn" href="/install/{slug}">{label}</a>
</div>"#,
            slug = slug,
            label = html_escape(&action_label_install),
            help = html_escape(&action_help_missing),
        )
    };

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
<p style="margin: 0 0 0.35rem; font-weight: 600; font-size: 0.85rem;">{identity_label}</p>
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.82rem;">{identity_help}</p>
<div style="display: flex; flex-direction: column; gap: 0.5rem;">
<label for="{slug}__idp_kind" style="font-size: 0.82rem;">Upstream IdP</label>
<select id="{slug}__idp_kind" name="{slug}__idp_kind" class="cz-select"{disabled_attr}>
<option value="">None (local-only auth)</option>
<option value="entra-id">Microsoft Entra ID</option>
<option value="aws-iam">AWS IAM Identity Center</option>
<option value="gcp-iam">GCP Identity Platform</option>
<option value="keycloak">Keycloak</option>
<option value="generic-oidc">Generic OIDC</option>
</select>
<label for="{slug}__idp_discovery_url" style="font-size: 0.82rem;">OIDC discovery URL</label>
<input id="{slug}__idp_discovery_url" name="{slug}__idp_discovery_url" class="cz-input" type="url" placeholder="https://login.microsoftonline.com/&lt;tenant&gt;/v2.0/.well-known/openid-configuration"{disabled_attr} />
<label for="{slug}__idp_client_id" style="font-size: 0.82rem;">Client ID</label>
<input id="{slug}__idp_client_id" name="{slug}__idp_client_id" class="cz-input" type="text" placeholder="computeza-{slug}"{disabled_attr} />
<label for="{slug}__idp_secret_ref" style="font-size: 0.82rem;">Client-secret ref (into the secrets store)</label>
<input id="{slug}__idp_secret_ref" name="{slug}__idp_secret_ref" class="cz-input" type="text" placeholder="{slug}/idp-client-secret"{disabled_attr} />
<label for="{slug}__idp_redirect_uri" style="font-size: 0.82rem;">Redirect URI</label>
<input id="{slug}__idp_redirect_uri" name="{slug}__idp_redirect_uri" class="cz-input" type="url" placeholder="https://console.example.com/auth/callback"{disabled_attr} />
<p class="cz-muted" style="margin: 0.35rem 0 0; font-size: 0.78rem;">Leave the IdP dropdown on "None" to skip federation for this component. Claim-to-group mappings configure on the /admin/operators page after first sign-on.</p>
</div>
</div>
{action_block}
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
        port_label = html_escape(&port_label),
        data_dir_label = html_escape(&data_dir_label),
        service_name_label = html_escape(&service_name_label),
        version_label = html_escape(&version_label),
        action_block = action_block,
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
pub fn render_install_hub(
    localizer: &Localizer,
    active: &[ActiveJob],
    installed: &std::collections::HashSet<String>,
) -> String {
    let title = localizer.t("ui-install-hub-title");
    let intro = localizer.t("ui-install-hub-intro");
    let install_all_button = localizer.t("ui-install-all-button");
    let install_all_helper = localizer.t("ui-install-all-helper");

    let cards: String = COMPONENTS
        .iter()
        .map(|c| render_unified_component_card(localizer, c, installed.contains(c.slug)))
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
            .map(|(i, v)| VersionOption {
                value: (*v).into(),
                label: format!("Garage {}{}", v, if i == 0 { " (latest)" } else { "" }),
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
    // Title flips for the rollback flow so the operator's tab + page
    // chrome don't say "Installing" while the system is tearing down.
    let title = if p.is_rollback {
        "Uninstalling".to_string()
    } else {
        localizer.t("ui-install-title")
    };
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
    let (hero_title, hero_intro) = if p.is_rollback {
        (
            "Uninstalling".to_string(),
            "Each component below is being torn down in reverse install order. You can leave this page open; it polls the server every half second and survives browser refresh.".to_string(),
        )
    } else if multi {
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
            r#"<section class="cz-section">
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
<section class="cz-section">
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
        r##"<section class="cz-hero" style="text-align: center;">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 36rem; margin: 0 auto;">
<div class="cz-card">
<h2 style="margin-top: 0; font-size: 1rem;">Identity</h2>
<dl class="cz-dl">
<dt>{username_label}</dt><dd><code>{username}</code></dd>
<dt>{session_since}</dt><dd><code data-ts-utc="{created_at}">{created_at}</code></dd>
</dl>
<form method="post" action="/logout" style="margin-top: 1.25rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">{logout}</button>
</form>
</div>

<div class="cz-card" style="margin-top: 1rem;">
<h2 style="margin-top: 0; font-size: 1rem;">Display preferences</h2>
<p class="cz-muted" style="margin: 0 0 1rem; font-size: 0.85rem; line-height: 1.5;">Controls how timestamps render across Computeza. Persisted in this browser only -- doesn't affect other operators or other devices. Server-side audit + log files always remain UTC for forensic consistency.</p>
<fieldset id="cz-tz-mode-fieldset" style="border: 1px solid var(--line); border-radius: var(--radius); padding: 0.85rem 1rem; margin: 0;">
<legend style="font-size: 0.75rem; color: var(--muted); padding: 0 0.4rem;">Timestamp display</legend>
<label style="display: flex; align-items: center; gap: 0.6rem; padding: 0.35rem 0; cursor: pointer;">
<input type="radio" name="cz-tz-mode" value="utc" checked />
<span><strong>UTC</strong> — default; matches audit log entries exactly.</span>
</label>
<label style="display: flex; align-items: center; gap: 0.6rem; padding: 0.35rem 0; cursor: pointer;">
<input type="radio" name="cz-tz-mode" value="local" />
<span><strong>Local time</strong> — convert UTC to this browser's timezone (<code id="cz-tz-detected">detecting…</code>).</span>
</label>
</fieldset>
<p class="cz-muted" style="margin: 0.85rem 0 0; font-size: 0.78rem;">Status: <strong id="cz-tz-status">UTC</strong></p>
</div>
</section>

<script>
// Detect the browser timezone and reflect the persisted preference.
// Stored under cz-tz-mode = "utc" | "local"; the global rewriter
// (in the shell footer) reads the same key on every page so the
// preference applies everywhere there's a [data-ts-utc] element.
(function () {{
  var KEY = "cz-tz-mode";
  var detectedEl = document.getElementById("cz-tz-detected");
  var statusEl = document.getElementById("cz-tz-status");
  var tz;
  try {{ tz = Intl.DateTimeFormat().resolvedOptions().timeZone; }} catch (_) {{}}
  if (detectedEl) detectedEl.textContent = tz || "browser default";
  var current = "utc";
  try {{ current = localStorage.getItem(KEY) || "utc"; }} catch (_) {{}}
  document.querySelectorAll('input[name="cz-tz-mode"]').forEach(function (r) {{
    r.checked = (r.value === current);
    r.addEventListener("change", function () {{
      try {{ localStorage.setItem(KEY, r.value); }} catch (_) {{}}
      paintStatus();
      // Re-run the page-wide rewriter so existing timestamps update
      // without a refresh.
      if (window.czRewriteTimestamps) window.czRewriteTimestamps();
    }});
  }});
  function paintStatus() {{
    var mode = "utc";
    try {{ mode = localStorage.getItem(KEY) || "utc"; }} catch (_) {{}}
    if (!statusEl) return;
    statusEl.textContent = mode === "local" ? ("Local (" + (tz || "browser") + ")") : "UTC";
  }}
  paintStatus();
}})();
</script>"##,
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
            let checkboxes = group_checkboxes_html(
                &format!("row-{}", op.username),
                &op.groups,
            );
            let _ = groups_str;
            format!(
                r#"<tr>
<td><code>{username}</code></td>
<td>
<form method="post" action="/admin/operators/{username_enc}/groups" style="display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap;">
<input type="hidden" name="csrf_token" value="" />
{checkboxes}
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
                checkboxes = checkboxes,
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
<span class="cz-input-label">{new_groups}</span>
<div style="display: flex; gap: 0.85rem; flex-wrap: wrap; padding: 0.3rem 0 0.6rem;">
{new_group_checkboxes}
</div>
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
        new_group_checkboxes = group_checkboxes_html("new", &["operators".to_string()]),
        new_submit = html_escape(&new_submit),
    );
    render_shell(localizer, &title, NavLink::Operators, &body)
}

/// Render one checkbox per [`auth::BUILTIN_GROUPS`] entry, pre-
/// checked when the group is in `current`. The boxes share the
/// `name="groups"` attribute so the browser submits one
/// `groups=<name>` pair per checked box -- our form handlers
/// flatten the resulting `Vec<String>` into a clean group list.
///
/// Replacing a free-text "operators,viewers" input with checkboxes
/// removes the entire "typo'd into a non-existent group" failure
/// mode (operators that match no known group end up with no
/// permissions and get locked out).
fn group_checkboxes_html(prefix: &str, current: &[String]) -> String {
    auth::BUILTIN_GROUPS
        .iter()
        .map(|(name, _perms)| {
            let id = format!("{prefix}-group-{name}");
            let checked = if current.iter().any(|g| g == name) {
                " checked"
            } else {
                ""
            };
            format!(
                r#"<label for="{id}" style="display: inline-flex; gap: 0.35rem; align-items: center; margin: 0; font-weight: normal; cursor: pointer;"><input id="{id}" type="checkbox" name="groups" value="{name}"{checked} /> <code>{name}</code></label>"#,
                id = html_escape(&id),
                name = html_escape(name),
                checked = checked,
            )
        })
        .collect()
}

/// Render `GET /admin/studios` -- v0.0.x list. Always shows the
/// implicit `default` studio.
#[must_use]
pub fn render_admin_studios(localizer: &Localizer) -> String {
    let title = localizer.t("ui-admin-tenants-title");
    let intro = localizer.t("ui-admin-tenants-intro");
    let col_name = localizer.t("ui-admin-tenants-col-name");
    let col_created = localizer.t("ui-admin-tenants-col-created");
    let note = localizer.t("ui-admin-tenants-default-note");

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>{col_name}</th><th>{col_created}</th></tr></thead>
<tbody>
<tr><td><code>default</code></td><td class="cz-cell-mono cz-cell-dim">(implicit)</td></tr>
</tbody>
</table>
<p class="cz-muted" style="margin: 0.85rem 0 0; font-size: 0.85rem;">{note}</p>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        col_name = html_escape(&col_name),
        col_created = html_escape(&col_created),
        note = html_escape(&note),
    );
    render_shell(localizer, &title, NavLink::Tenants, &body)
}

/// Render `GET /admin/branding` -- the white-labeling form.
#[must_use]
pub fn render_admin_branding(
    localizer: &Localizer,
    current_accent: Option<&str>,
    error_message: Option<&str>,
) -> String {
    let title = localizer.t("ui-admin-branding-title");
    let intro = localizer.t("ui-admin-branding-intro");
    let label = localizer.t("ui-admin-branding-accent");
    let help = localizer.t("ui-admin-branding-accent-help");
    let submit = localizer.t("ui-admin-branding-submit");
    let reset = localizer.t("ui-admin-branding-reset");

    let value = current_accent.unwrap_or("");
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
        r##"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section" style="max-width: 32rem;">
{error_block}
<div class="cz-card">
<form method="post" action="/admin/branding" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<label for="accent">{label}</label>
<input id="accent" name="accent" class="cz-input" type="text" value="{value}" placeholder="#C4B8E8" pattern="^(#[A-Fa-f0-9]{{3}}|#[A-Fa-f0-9]{{6}})?$" />
<p class="cz-muted" style="margin: -0.5rem 0 0; font-size: 0.8rem;">{help}</p>
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">{submit}</button>
</form>
</div>
<p class="cz-muted" style="margin-top: 0.75rem; font-size: 0.85rem;">Leave the field blank and submit to {reset_lower}.</p>
</section>"##,
        title = html_escape(&title),
        intro = html_escape(&intro),
        error_block = error_block,
        label = html_escape(&label),
        help = html_escape(&help),
        value = html_escape(value),
        submit = html_escape(&submit),
        reset_lower = html_escape(&reset.to_lowercase()),
    );
    render_shell(localizer, &title, NavLink::Branding, &body)
}

/// Render `GET /compliance/eu-ai-act` -- overview of the deployer's
/// obligations + Article 9-72 checklist.
#[must_use]
pub fn render_compliance_eu_ai_act(localizer: &Localizer) -> String {
    let title = "EU AI Act compliance";
    let intro = "Computeza supports deployer-side compliance with Regulation (EU) 2024/1689. \
                 High-risk obligations take effect on 2 August 2026; the checklist below maps each \
                 Article to the artefacts Computeza captures automatically and the ones the \
                 deployer must produce.";
    let positioning = "Important: Computeza is a tooling vendor, not a co-deployer. Activating \
                       these features helps you produce evidence -- it does not by itself make \
                       your AI system Act-compliant. Marketing language matters legally here.";
    let articles = computeza_compliance::eu_ai_act_articles();
    let rows: String = articles
        .iter()
        .map(|a| {
            format!(
                r#"<tr>
<td><strong>Art {article}</strong></td>
<td>{title}</td>
<td class="cz-muted">{obligation}</td>
<td class="cz-muted" style="font-size: 0.78rem;">{evidence}</td>
</tr>"#,
                article = html_escape(a.article),
                title = html_escape(a.title),
                obligation = html_escape(a.obligation),
                evidence = html_escape(a.evidence_source),
            )
        })
        .collect();
    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0;">{positioning}</p>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.6rem;">Article checklist</h3>
<table class="cz-table" style="width: 100%;">
<thead><tr><th>Article</th><th>Title</th><th>Obligation</th><th>Evidence source</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Next step</h3>
<p class="cz-muted" style="margin: 0 0 0.8rem; font-size: 0.85rem;">Register a model card for each AI system in your studio. The registry produces the Annex IV technical documentation per system, links it to the audit log, and flags Prohibited classifications before they can deploy.</p>
<a href="/compliance/models" class="cz-btn cz-btn-primary">Open the model card registry</a>
</div>
</section>"#,
        title = html_escape(title),
        intro = html_escape(intro),
        positioning = html_escape(positioning),
        rows = rows,
    );
    render_shell(localizer, title, NavLink::Compliance, &body)
}

/// Render `GET /compliance/models` -- listing of all registered
/// cards + a create form. When `registry_attached` is false, the
/// page shows a smoke-test explanation card.
#[must_use]
pub fn render_compliance_models_list(
    localizer: &Localizer,
    cards: &[computeza_compliance::ModelCard],
    registry_attached: bool,
    error_message: Option<&str>,
) -> String {
    let title = "Model card registry";
    let intro = "Annex IV technical documentation per AI system, persisted as JSONL next to the \
                 state DB. One row per system; the deployer's classification cascades into the \
                 Article 9-15 evidence checklist.";
    let error_block = error_message
        .map(|msg| {
            format!(
                r#"<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55); margin-bottom: 1rem;">
<p class="cz-card-body" style="margin: 0; color: var(--fail);">{}</p>
</div>"#,
                html_escape(msg)
            )
        })
        .unwrap_or_default();
    if !registry_attached {
        let body = format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0;">No model-card registry attached on this server. The real binary opens one at <code>&lt;state_db_parent&gt;/model-cards.jsonl</code> at boot.</p>
</div>
</section>"#,
            title = html_escape(title),
            intro = html_escape(intro),
        );
        return render_shell(localizer, title, NavLink::Compliance, &body);
    }
    let rows: String = if cards.is_empty() {
        r#"<tr><td colspan="4" class="cz-muted" style="text-align: center; padding: 1.5rem;">No cards registered yet. Use the form below to register the first one.</td></tr>"#.into()
    } else {
        cards
            .iter()
            .map(|c| {
                let badge_class = match c.risk {
                    computeza_compliance::RiskClassification::HighRisk => "cz-badge cz-badge-warn",
                    computeza_compliance::RiskClassification::LimitedRisk => "cz-badge",
                    computeza_compliance::RiskClassification::Minimal => "cz-badge",
                    computeza_compliance::RiskClassification::Prohibited => "cz-badge",
                };
                let complete = if matches!(c.risk, computeza_compliance::RiskClassification::HighRisk) {
                    if c.high_risk_evidence_complete() {
                        r#"<span class="cz-badge" style="background: rgba(95, 207, 156, 0.18);">Art 9-15 complete</span>"#
                    } else {
                        r#"<span class="cz-badge cz-badge-warn">Art 9-15 incomplete</span>"#
                    }
                } else {
                    ""
                };
                format!(
                    r#"<tr>
<td><a href="/compliance/models/{id_enc}"><code>{id}</code></a></td>
<td>{name}</td>
<td><span class="{badge_class}">{risk}</span> {complete}</td>
<td class="cz-cell-mono cz-cell-dim">{updated}</td>
</tr>"#,
                    id = html_escape(&c.id),
                    id_enc = urlencoding_min(&c.id),
                    name = html_escape(&c.name),
                    risk = html_escape(c.risk.label()),
                    badge_class = badge_class,
                    complete = complete,
                    updated = html_escape(&c.updated_at.to_rfc3339()),
                )
            })
            .collect()
    };
    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{error_block}
<section class="cz-section">
<div class="cz-card">
<table class="cz-table" style="width: 100%;">
<thead><tr><th>ID</th><th>Name</th><th>Risk</th><th>Updated</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Register a model card</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">Prohibited classifications (Article 5) are refused -- remove the system before registering. High-risk cards auto-surface the Article 9-15 evidence checklist on the detail page.</p>
<form method="post" action="/compliance/models" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<label for="card-id">ID (stable slug -- UUID, kebab-case slug, etc.)</label>
<input id="card-id" name="id" class="cz-input" type="text" required pattern="[A-Za-z0-9_\-]+" maxlength="64" />
<label for="card-name">Display name</label>
<input id="card-name" name="name" class="cz-input" type="text" required maxlength="200" />
<label for="card-risk">Risk classification</label>
<select id="card-risk" name="risk" class="cz-input" required>
<option value="">-- select --</option>
<option value="high-risk">High-risk (Annex III)</option>
<option value="limited-risk">Limited risk (Article 50)</option>
<option value="minimal">Minimal risk</option>
</select>
<label for="card-rationale">Risk justification rationale</label>
<textarea id="card-rationale" name="rationale" class="cz-input" rows="3" required></textarea>
<label for="card-intended">Intended use (Article 13)</label>
<textarea id="card-intended" name="intended_use" class="cz-input" rows="3" required></textarea>
<label for="card-training">Training data summary (Article 10)</label>
<textarea id="card-training" name="training_data_summary" class="cz-input" rows="3" required></textarea>
<label for="card-limits">Known limitations (Article 13)</label>
<textarea id="card-limits" name="limitations" class="cz-input" rows="3" required></textarea>
<label for="card-oversight">Human oversight design (Article 14)</label>
<textarea id="card-oversight" name="human_oversight_design" class="cz-input" rows="3" required></textarea>
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">Register card</button>
</form>
</div>
</section>"#,
        title = html_escape(title),
        intro = html_escape(intro),
        error_block = error_block,
        rows = rows,
    );
    render_shell(localizer, title, NavLink::Compliance, &body)
}

/// Render `GET /compliance/models/{id}` -- single card detail.
#[must_use]
pub fn render_compliance_model_detail(
    localizer: &Localizer,
    card: &computeza_compliance::ModelCard,
) -> String {
    let evidence_table = if matches!(
        card.risk,
        computeza_compliance::RiskClassification::HighRisk
    ) {
        let articles = computeza_compliance::eu_ai_act_articles();
        let rows: String = articles
            .iter()
            .filter(|a| matches!(a.article, "9" | "10" | "11" | "12" | "13" | "14" | "15"))
            .map(|a| {
                let attached: Vec<_> = card
                    .article_evidence
                    .iter()
                    .filter(|e| e.article == a.article)
                    .collect();
                let status = if attached.is_empty() {
                    r#"<span class="cz-badge cz-badge-warn">No evidence yet</span>"#.to_string()
                } else {
                    format!(
                        r#"<span class="cz-badge">{} artefact(s)</span>"#,
                        attached.len()
                    )
                };
                format!(
                    r#"<tr><td><strong>Art {art}</strong></td><td>{title}</td><td>{status}</td></tr>"#,
                    art = html_escape(a.article),
                    title = html_escape(a.title),
                    status = status,
                )
            })
            .collect();
        format!(
            r#"<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.5rem;">Article 9-15 evidence checklist</h3>
<table class="cz-table" style="width: 100%;"><thead><tr><th>Article</th><th>Title</th><th>Evidence</th></tr></thead><tbody>{rows}</tbody></table>
</div>
</section>"#,
            rows = rows,
        )
    } else {
        String::new()
    };
    let title = format!("Model card: {}", card.name);
    let body = format!(
        r#"<section class="cz-hero">
<h1>{name}</h1>
<p><code>{id}</code> &middot; <span class="cz-badge">{risk}</span></p>
</section>
<section class="cz-section">
<div class="cz-card">
<dl class="cz-dl">
<dt>Intended use (Art 13)</dt><dd>{intended}</dd>
<dt>Training data summary (Art 10)</dt><dd>{training}</dd>
<dt>Limitations (Art 13)</dt><dd>{limits}</dd>
<dt>Human oversight design (Art 14)</dt><dd>{oversight}</dd>
<dt>Risk justification</dt><dd>{rationale}</dd>
<dt>Registered</dt><dd><code>{created}</code></dd>
<dt>Last update</dt><dd><code>{updated}</code></dd>
</dl>
</div>
</section>
{evidence_table}
<section class="cz-section">
<form method="post" action="/compliance/models/{id_enc}/delete" onsubmit="return confirm('Delete this model card? The audit log retains the registration + deletion events.');" style="margin: 0;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">Delete card</button>
</form>
</section>
<section class="cz-section">
<a href="/compliance/models" class="cz-btn">Back to registry</a>
</section>"#,
        name = html_escape(&card.name),
        id = html_escape(&card.id),
        id_enc = urlencoding_min(&card.id),
        risk = html_escape(card.risk.label()),
        intended = html_escape(&card.intended_use),
        training = html_escape(&card.training_data_summary),
        limits = html_escape(&card.limitations),
        oversight = html_escape(&card.human_oversight_design),
        rationale = html_escape(&card.risk_justification.rationale),
        created = html_escape(&card.created_at.to_rfc3339()),
        updated = html_escape(&card.updated_at.to_rfc3339()),
        evidence_table = evidence_table,
    );
    render_shell(localizer, &title, NavLink::Compliance, &body)
}

/// Render `GET /admin/pq-status` -- the post-quantum readiness
/// dashboard. Pairs the transport-layer report from
/// [`computeza_channel_partner::pq::tls_readiness`] with the license
/// envelope's PQ posture so the operator sees both surfaces in one
/// place.
#[must_use]
pub fn render_admin_pq_status(
    localizer: &Localizer,
    pq: &computeza_channel_partner::pq::PqReadiness,
    license: Option<&computeza_license::License>,
) -> String {
    let title = "Post-quantum readiness";
    let intro = "Computeza tracks two independent post-quantum surfaces: the TLS transport \
                 (handshakes today, harvest-now-decrypt-later resistance) and the license \
                 envelope (dual-classical+ML-DSA signature shape, verified in v0.1).";
    let yes = r#"<span class="cz-badge" style="background: rgba(95, 207, 156, 0.18); color: var(--ok, #5fcf9c);">Yes</span>"#;
    let no = r#"<span class="cz-badge" style="background: rgba(255, 196, 87, 0.18); color: var(--warn);">No</span>"#;
    let pq_pair = |b: bool| -> &'static str {
        if b {
            yes
        } else {
            no
        }
    };

    let license_section = match license {
        None => format!(
            r#"<dt>License envelope</dt><dd>{no} (Community mode -- no envelope activated)</dd>"#,
            no = no,
        ),
        Some(lic) => {
            let dual = lic.is_pq_dual_signed();
            format!(
                r#"<dt>License envelope dual-signed</dt><dd>{badge}</dd>
<dt>License id</dt><dd><code>{id}</code></dd>"#,
                badge = pq_pair(dual),
                id = html_escape(&lic.payload.id),
            )
        }
    };

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.5rem;">Transport (TLS / mTLS)</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">Hybrid key-exchange means the session key requires breaking BOTH X25519 and ML-KEM -- an attacker that records the handshake today cannot decrypt it after a quantum break of either primitive alone.</p>
<dl class="cz-dl">
<dt>Crypto provider</dt><dd><code>{provider}</code></dd>
<dt>Hybrid KEX enabled</dt><dd>{kex_enabled}</dd>
<dt>Hybrid group offered</dt><dd><code>{kex_group}</code></dd>
</dl>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.5rem;">License signatures</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">Dual-signature envelopes carry both Ed25519 (classical, today) and ML-DSA (FIPS 204 / Dilithium, post-quantum). v0.0.x verifies shape only; v0.1 wires the cryptographic ML-DSA verify alongside Ed25519.</p>
<dl class="cz-dl">
<dt>Dual-sig envelope shape supported</dt><dd>{dual_supported}</dd>
<dt>ML-DSA cryptographic verification active</dt><dd>{dual_verified}</dd>
{license_section}
</dl>
</div>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0;"><strong>Roadmap:</strong> v0.1 brings (a) ML-DSA cryptographic verification on the license leg via a vetted pure-Rust crate, (b) dual-classical+ML-DSA X.509 issuance for the channel-partner mTLS material, and (c) explicit reporting in the audit log when a downgrade-to-classical handshake occurs.</p>
</div>
</section>"#,
        title = html_escape(title),
        intro = html_escape(intro),
        provider = html_escape(pq.crypto_provider),
        kex_enabled = pq_pair(pq.tls_hybrid_kex_enabled),
        kex_group = html_escape(pq.tls_hybrid_kex_group),
        dual_supported = pq_pair(pq.license_dual_sig_supported),
        dual_verified = pq_pair(pq.license_dual_sig_verified),
        license_section = license_section,
    );
    render_shell(localizer, title, NavLink::License, &body)
}

/// Render the one-time-view page produced by the
/// "Generate passphrase" button on `/admin/secrets`. The passphrase
/// is shown verbatim with copy-to-clipboard affordance and a ready-
/// made systemd EnvironmentFile snippet. Computeza does not retain
/// the passphrase -- this is the operator's only chance to grab it.
#[must_use]
pub fn render_secrets_setup_generated(localizer: &Localizer, passphrase: &str) -> String {
    let title = localizer.t("ui-secrets-title");
    let pphr_esc = html_escape(passphrase);
    let body = format!(
        r#"<section class="cz-hero">
<h1>Passphrase generated</h1>
<p>Copy the value below into your environment before navigating away. Computeza did not save it -- if you lose this tab, click Generate again to mint a fresh one (every newly-derived KEK requires re-encrypting any previously-stored secrets, so generate now and back up before storing anything sensitive).</p>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0;"><span class="cz-badge cz-badge-warn">One-time view</span>&nbsp; Treat this passphrase like a root password. Anyone with it + the salt file + the ciphertext file recovers every stored secret.</p>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Your passphrase</h3>
<div style="display: flex; gap: 0.5rem; align-items: stretch;">
<input id="cz-pphr" type="text" readonly value="{passphrase}" class="cz-input" style="font-family: ui-monospace, monospace; font-size: 0.85rem; flex: 1; margin: 0;" />
<button type="button" class="cz-btn" onclick="(function(){{var i=document.getElementById('cz-pphr'); i.select(); navigator.clipboard.writeText(i.value); this.textContent='Copied'; setTimeout(function(){{document.querySelector('button[onclick*=cz-pphr]').textContent='Copy';}},2000);}}).call(this)">Copy</button>
</div>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">systemd drop-in template</h3>
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.85rem;">Drop the env file into <code>/etc/computeza/</code> with restrictive permissions, then reference it from your <code>computeza.service</code> unit via <code>EnvironmentFile=</code>. Reload + restart for the change to take effect.</p>
<pre style="background: var(--surface-2, #161b22); padding: 0.85rem; border-radius: 0.4rem; font-size: 0.78rem; overflow-x: auto; margin: 0;">sudo install -d -m 0750 -o root -g root /etc/computeza
sudo tee /etc/computeza/computeza.env &gt;/dev/null &lt;&lt;'EOF'
COMPUTEZA_SECRETS_PASSPHRASE={passphrase}
EOF
sudo chmod 0640 /etc/computeza/computeza.env
sudo chown root:root /etc/computeza/computeza.env

# In computeza.service (or via systemctl edit computeza.service):
#   [Service]
#   EnvironmentFile=/etc/computeza/computeza.env

sudo systemctl daemon-reload
sudo systemctl restart computeza</pre>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Inline shell (dev / one-off)</h3>
<pre style="background: var(--surface-2, #161b22); padding: 0.85rem; border-radius: 0.4rem; font-size: 0.78rem; overflow-x: auto; margin: 0;">export COMPUTEZA_SECRETS_PASSPHRASE={passphrase}
computeza serve --addr 127.0.0.1:8400 --state-db /var/lib/computeza/state.db</pre>
</div>
</section>
<section class="cz-section">
<div class="cz-card" style="border-color: rgba(245, 181, 68, 0.55);">
<p class="cz-card-body" style="margin: 0;"><strong>Back up THREE things together:</strong> this passphrase, the salt file (<code>computeza-secrets.salt</code>), and the ciphertext file (<code>computeza-secrets.jsonl</code>). Losing any one of them = permanent secret loss.</p>
</div>
</section>
<section class="cz-section">
<a href="/admin/secrets" class="cz-btn">Return to secrets</a>
</section>"#,
        passphrase = pphr_esc,
    );
    let _ = title;
    render_shell(localizer, "Passphrase generated", NavLink::Secrets, &body)
}

/// Render `GET /admin/license` -- live license envelope viewer +
/// activation form. `status` is the live verification result; the
/// page surfaces a renewal banner when `status.should_warn()` and
/// switches the activate-form into a "Replace license" affordance
/// when one is already active. `seat_usage` shows the live count of
/// operator accounts (rendered when the license carries a seat cap).
#[must_use]
pub fn render_admin_license_v2(
    localizer: &Localizer,
    license: Option<&computeza_license::License>,
    status: computeza_license::LicenseStatus,
    seat_usage: Option<usize>,
    error_message: Option<&str>,
) -> String {
    let title = localizer.t("ui-admin-license-title");
    let intro = localizer.t("ui-admin-license-intro");
    let none = localizer.t("ui-admin-license-none");
    let tier_label = localizer.t("ui-admin-license-tier");
    let seats_label = localizer.t("ui-admin-license-seats");
    let chain_label = localizer.t("ui-admin-license-chain");
    let not_before_label = localizer.t("ui-admin-license-not-before");
    let not_after_label = localizer.t("ui-admin-license-not-after");

    let status_banner = render_license_status_banner(&status);
    let error_block = error_message
        .map(|msg| {
            format!(
                r#"<div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55); margin-bottom: 1rem;">
<p class="cz-card-body" style="margin: 0; color: var(--fail);">{}</p>
</div>"#,
                html_escape(msg)
            )
        })
        .unwrap_or_default();

    let activation_form = render_license_activation_form(license.is_some());
    let pq_link = r#"<p class="cz-muted" style="margin: 0.6rem 0 0; font-size: 0.85rem;"><a href="/admin/pq-status">View the post-quantum readiness dashboard -&gt;</a></p>"#;
    let deactivation_form = if license.is_some() {
        r#"<form method="post" action="/admin/license/deactivate" onsubmit="return confirm('Drop the active license and return to Community mode? Mutating routes will require a fresh envelope to re-enable.');" style="margin-top: 0.8rem;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-danger">Deactivate license</button>
</form>"#
            .to_string()
    } else {
        String::new()
    };

    let body = match license {
        None => format!(
            r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{status_banner}
<section class="cz-section">
<div class="cz-card"><p class="cz-card-body" style="margin: 0;">{none}</p></div>
</section>
<section class="cz-section">
{error_block}
{activation_form}
{pq_link}
</section>"#,
            title = html_escape(&title),
            intro = html_escape(&intro),
            none = html_escape(&none),
            status_banner = status_banner,
            error_block = error_block,
            activation_form = activation_form,
            pq_link = pq_link,
        ),
        Some(lic) => {
            let chain_rows: String = lic
                .payload
                .chain
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let support = entry
                        .support_contact
                        .as_deref()
                        .map(|s| format!(" -- support: <code>{}</code>", html_escape(s)))
                        .unwrap_or_default();
                    format!(
                        r#"<li>Tier {i}: <strong>{name}</strong>{support}</li>"#,
                        name = html_escape(&entry.name),
                    )
                })
                .collect();
            let seats_value = match (lic.payload.seats, seat_usage) {
                (Some(cap), Some(used)) => format!("{used} / {cap}"),
                (Some(cap), None) => format!("{cap}"),
                (None, _) => "unlimited".into(),
            };
            let features_block = if lic.payload.features.is_empty() {
                String::new()
            } else {
                let chips: String = lic
                    .payload
                    .features
                    .iter()
                    .map(|f| {
                        format!(
                            r#"<span class="cz-badge" style="margin-right: 0.3rem;"><code>{}</code></span>"#,
                            html_escape(f)
                        )
                    })
                    .collect();
                format!("<dt>Features</dt><dd>{chips}</dd>")
            };
            let billing_block = match &lic.payload.billing_metadata {
                None => String::new(),
                Some(b) => {
                    let amount = match (b.annual_value, b.currency.as_deref()) {
                        (Some(v), Some(cur)) => format!("{:.2} {cur} / year", (v as f64) / 100.0),
                        _ => "(unspecified)".into(),
                    };
                    let contract = b
                        .contract_id
                        .as_deref()
                        .map(|c| {
                            format!("<dt>Contract</dt><dd><code>{}</code></dd>", html_escape(c))
                        })
                        .unwrap_or_default();
                    format!(
                        "<dt>Annual value</dt><dd>{amount}</dd>{contract}",
                        amount = html_escape(&amount),
                        contract = contract,
                    )
                }
            };
            format!(
                r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{status_banner}
<section class="cz-section">
<div class="cz-card">
<dl class="cz-dl">
<dt>{tier}</dt><dd>{tier_value}</dd>
<dt>{seats}</dt><dd>{seats_value}</dd>
<dt>{nb}</dt><dd><code>{nb_value}</code></dd>
<dt>{na}</dt><dd><code>{na_value}</code></dd>
{features_block}
{billing_block}
</dl>
<h3 style="margin-top: 1rem;">{chain}</h3>
<ol class="cz-muted" style="margin-top: 0.4rem; padding-left: 1.2rem;">{rows}</ol>
{deactivation_form}
</div>
</section>
<section class="cz-section">
{error_block}
{activation_form}
{pq_link}
</section>"#,
                title = html_escape(&title),
                intro = html_escape(&intro),
                status_banner = status_banner,
                tier = html_escape(&tier_label),
                tier_value = html_escape(&lic.payload.tier),
                seats = html_escape(&seats_label),
                seats_value = html_escape(&seats_value),
                nb = html_escape(&not_before_label),
                nb_value = html_escape(&lic.payload.not_before.to_rfc3339()),
                na = html_escape(&not_after_label),
                na_value = html_escape(&lic.payload.not_after.to_rfc3339()),
                features_block = features_block,
                billing_block = billing_block,
                chain = html_escape(&chain_label),
                rows = chain_rows,
                deactivation_form = deactivation_form,
                error_block = error_block,
                activation_form = activation_form,
            )
        }
    };
    render_shell(localizer, &title, NavLink::License, &body)
}

/// Compatibility shim for callers that still want the legacy
/// signature (tests, smoke router). Always shows community-mode card
/// when `license = None`; rendered status defaults to `None`.
#[must_use]
pub fn render_admin_license(
    localizer: &Localizer,
    license: Option<&computeza_license::License>,
) -> String {
    let status = match license {
        None => computeza_license::LicenseStatus::None,
        Some(_) => computeza_license::LicenseStatus::Active {
            days_remaining: i64::MAX / 86_400,
        },
    };
    render_admin_license_v2(localizer, license, status, None, None)
}

fn render_license_activation_form(replacing: bool) -> String {
    let heading = if replacing {
        "Replace the active license"
    } else {
        "Activate a license"
    };
    let help = if replacing {
        "Paste a freshly-issued envelope below. The new license is verified against the binary's trusted root + the current time before it replaces the active one; a bad envelope leaves the existing license in place."
    } else {
        "Paste the JSON envelope your reseller / Computeza sales issued for this install. The envelope is verified against the binary's trusted root + the current time before it is persisted."
    };
    let button = if replacing {
        "Replace license"
    } else {
        "Activate license"
    };
    format!(
        r#"<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">{heading}</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">{help}</p>
<form method="post" action="/admin/license/activate" class="cz-form" style="max-width: none;">
<input type="hidden" name="csrf_token" value="" />
<label for="license-envelope">License envelope (JSON)</label>
<textarea id="license-envelope" name="envelope" rows="10" class="cz-input" style="font-family: ui-monospace, monospace; font-size: 0.82rem;" required></textarea>
<button type="submit" class="cz-btn cz-btn-primary" style="margin-top: 0.5rem;">{button}</button>
</form>
</div>"#,
        heading = html_escape(heading),
        help = html_escape(help),
        button = html_escape(button),
    )
}

fn render_license_status_banner(status: &computeza_license::LicenseStatus) -> String {
    use computeza_license::LicenseStatus;
    if !status.should_warn() {
        return String::new();
    }
    match status {
        LicenseStatus::None => String::new(),
        LicenseStatus::Active { days_remaining } => format!(
            r#"<section class="cz-section"><div class="cz-card" style="border-color: rgba(255, 196, 87, 0.55);"><p class="cz-card-body" style="margin: 0;"><strong>Renewal due:</strong> the active license expires in <code>{days_remaining}</code> day(s). Contact your reseller / Computeza sales for a fresh envelope.</p></div></section>"#
        ),
        LicenseStatus::Expired { days_since_expiry } => format!(
            r#"<section class="cz-section"><div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55);"><p class="cz-card-body" style="margin: 0; color: var(--fail);"><strong>License expired {days_since_expiry} day(s) ago.</strong> The control plane is in read-only mode; the data plane keeps running. Paste a fresh envelope below to re-enable mutations.</p></div></section>"#
        ),
        LicenseStatus::NotYetValid { days_until_valid } => format!(
            r#"<section class="cz-section"><div class="cz-card" style="border-color: rgba(255, 196, 87, 0.55);"><p class="cz-card-body" style="margin: 0;"><strong>License not yet valid.</strong> Effective in {days_until_valid} day(s); the console is read-only until then.</p></div></section>"#
        ),
        LicenseStatus::Invalid(reason) => format!(
            r#"<section class="cz-section"><div class="cz-card" style="border-color: rgba(255, 157, 166, 0.55);"><p class="cz-card-body" style="margin: 0; color: var(--fail);"><strong>License invalid ({reason}).</strong> The control plane has fallen back to Community mode. Activate a valid envelope to restore entitlements.</p></div></section>"#
        ),
    }
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
    render_install_result_with_credentials(localizer, success, detail, &[], None, None)
}

/// Sibling of `render_install_result` for the rollback / uninstall
/// flow. Identical layout but the page title + outcome label say
/// "Uninstall" so the operator isn't confused by the install-flavored
/// chrome on what's really a teardown.
pub fn render_uninstall_result(localizer: &Localizer, success: bool, detail: &str) -> String {
    let mut body = render_install_result_with_credentials(
        localizer, success, detail, &[], None, None,
    );
    // Swap install-flavored strings for uninstall ones. Cheap
    // string-replace -- the localizer keys we'd otherwise add
    // (ui-uninstall-result-title etc.) would proliferate every time
    // an install-result tweak landed without a paired uninstall-
    // result tweak, so a focused fixup at the render edge keeps
    // the two pages in sync by construction.
    body = body
        .replace(">Install result<", ">Uninstall results<")
        .replace(">Install completed.<", ">Uninstall and removal of components completed.<")
        .replace(">Install failed.<", ">Uninstall completed with errors.<");
    body
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
    job_id_for_credentials_download: Option<&str>,
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
    let credentials_block =
        render_credentials_block(localizer, credentials, job_id_for_credentials_download);
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
<p class="cz-card-body" style="margin: 0 0 0.6rem; color: var(--warn);"><strong>No secrets store attached.</strong> {store_missing}</p>
<p class="cz-muted" style="margin: 0; font-size: 0.85rem;">Walk through the three-step wizard below to wire one up in under a minute. The control plane keeps running in degraded mode (in-band credentials only) until you do.</p>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Step 1 -- Generate a strong passphrase</h3>
<p class="cz-muted" style="margin: 0 0 0.85rem; font-size: 0.85rem;">Computeza emits 256 bits of CSPRNG entropy as a 64-character hex string. The passphrase is shown to you exactly once on the next page; copy it out before navigating away. Computeza never persists the passphrase.</p>
<form method="post" action="/admin/secrets/setup/generate-passphrase" style="margin: 0;">
<input type="hidden" name="csrf_token" value="" />
<button type="submit" class="cz-btn cz-btn-primary">Generate passphrase</button>
</form>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Step 2 -- Wire it into <code>computeza serve</code></h3>
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.85rem;">After generating: put the passphrase in your environment. The recommended shape is a systemd drop-in env file at <code>/etc/computeza/computeza.env</code> referenced by <code>EnvironmentFile=</code> in your service unit -- the template appears on the result page. For dev / one-off runs an inline <code>export</code> works too.</p>
<pre style="background: var(--surface-2, #161b22); padding: 0.85rem; border-radius: 0.4rem; font-size: 0.78rem; overflow-x: auto; margin: 0;">sudo install -d -m 0750 -o root -g root /etc/computeza
sudo install -m 0640 -o root -g root /dev/null /etc/computeza/computeza.env
sudo tee /etc/computeza/computeza.env &gt;/dev/null &lt;&lt;'EOF'
COMPUTEZA_SECRETS_PASSPHRASE=&lt;paste your generated passphrase here&gt;
EOF</pre>
</div>
</section>
<section class="cz-section">
<div class="cz-card">
<h3 style="margin: 0 0 0.4rem;">Step 3 -- Back up the disaster-recovery triple</h3>
<p class="cz-muted" style="margin: 0 0 0.6rem; font-size: 0.85rem;">Losing <em>any one</em> of these three renders every secret unrecoverable -- by design, no master recovery path:</p>
<ol class="cz-muted" style="margin: 0 0 0.6rem 1.2rem; font-size: 0.85rem;">
<li>the <code>COMPUTEZA_SECRETS_PASSPHRASE</code> value</li>
<li>the salt file (<code>computeza-secrets.salt</code>) next to the state DB</li>
<li>the ciphertext file (<code>computeza-secrets.jsonl</code>) next to the state DB</li>
</ol>
<p class="cz-muted" style="margin: 0; font-size: 0.85rem;">Restart <code>computeza serve</code> after editing the env file. This page will switch to the live secret list once the store attaches.</p>
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
    job_id: Option<&str>,
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

    // Optional one-shot JSON download. Only rendered when we know
    // the job_id (the unified-install flow always passes it; legacy
    // tests that call render_install_result_with_credentials with
    // None get the table without the button). Same view-once
    // contract as the on-page table: server drains the cache on
    // first hit, second click returns 410 Gone.
    let download_block = match job_id {
        Some(id) => format!(
            r#"<p style="margin: 0.85rem 0 0;"><a class="cz-btn" href="/install/credentials.json/{id}" download>Download credentials as JSON</a> <span class="cz-muted" style="margin-left: 0.6rem; font-size: 0.82rem;">One-shot: the file is removed from the server on first download.</span></p>"#,
            id = html_escape(id),
        ),
        None => String::new(),
    };

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
{download_block}
</div>
</section>"#,
        title = html_escape(&title),
        warning = html_escape(&warning),
        comp_h = html_escape(&comp_h),
        user_h = html_escape(&user_h),
        pass_h = html_escape(&pass_h),
        ref_h = html_escape(&ref_h),
        rows = rows,
        download_block = download_block,
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
            let state_not_installed = localizer.t("ui-status-state-not-installed");
            let body_rows: String = rs
                .iter()
                .map(|r| {
                    let not_installed = r.instance_name.is_empty();
                    let (state_label, badge_cls) = if not_installed {
                        (state_not_installed.clone(), "cz-badge cz-badge-dim")
                    } else if !r.has_status {
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
                    // Placeholder rows (no instance yet) skip the
                    // resource link -- it would 404 -- and surface
                    // just the component name with a dim "Not
                    // installed" badge so the catalogue is
                    // discoverable at a glance.
                    let name_cell = if not_installed {
                        format!(
                            "<span class=\"cz-strong\">{label}</span>",
                            label = html_escape(&r.component_label),
                        )
                    } else {
                        let href = format!(
                            "/resource/{}/{}",
                            urlencoding_min(&r.kind),
                            urlencoding_min(&r.instance_name)
                        );
                        format!(
                            "<a href=\"{href}\" class=\"cz-strong\">{label} / {name}</a>",
                            href = href,
                            label = html_escape(&r.component_label),
                            name = html_escape(&r.instance_name),
                        )
                    };
                    format!(
                        "<tr>\
                         <td class=\"cz-cell-mono cz-cell-dim\">{kind}</td>\
                         <td>{name_cell}</td>\
                         <td class=\"cz-cell-dim\">{version}</td>\
                         <td class=\"cz-cell-mono cz-cell-dim\">{observed}</td>\
                         <td><span class=\"{badge_cls}\">{state_label}</span></td>\
                         </tr>",
                        kind = html_escape(&r.kind),
                        name_cell = name_cell,
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
        let ws_label = localizer.t("ui-resource-tenant");
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
        let tenant = sr
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
<dt>{ws_label}</dt><dd>{tenant}</dd>
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
    fn render_install_guide_covers_all_sections() {
        // The install guide is the primary onboarding surface; the
        // sections below are the contract with operators arriving
        // from the marketing landing. If any one regresses, the
        // operator hits a half-documented onboarding path.
        let l = Localizer::english();
        let html = render_install_guide(&l);
        for marker in [
            "Install guide",
            r#"id="prereqs""#,
            r#"id="step-1-build""#,
            r#"id="step-2-first-run""#,
            r#"id="step-3-components""#,
            r#"id="step-4-license""#,
            r#"id="wsl""#,
            r#"id="hardening""#,
            r#"id="troubleshooting""#,
            r#"id="help""#,
            "COMPUTEZA_SECRETS_PASSPHRASE",
            "systemctl is-system-running",
            "get.enterprisedb.com",
            "/admin/license",
            "/install",
        ] {
            assert!(
                html.contains(marker),
                "install-guide page should contain {marker:?}; first 2 KiB of HTML:\n{}",
                &html[..html.len().min(2048)]
            );
        }
    }

    #[test]
    fn install_guide_is_reachable_via_router_unauthenticated() {
        // The route MUST be in PUBLIC_PATH_PREFIXES so a prospect
        // browsing the marketing site can read prereqs before
        // committing to signing up.
        assert!(crate::auth::is_public_path("/install-guide"));
    }

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
    fn render_shell_wires_password_show_hide_toggle() {
        // Smoke test: the inline JS that auto-decorates
        // <input type="password"> with a show/hide eye is wired
        // into the shared shell. Any page that renders through
        // render_shell (login, setup, admin/operators, secrets
        // rotate, etc.) gets it -- no per-page hookup needed.
        let l = Localizer::english();
        let html = render_login(&l, "/install", None);
        assert!(
            html.contains("dataset.czEyeBound"),
            "render_shell should embed the password-eye toggle JS"
        );
        assert!(
            html.contains(r#"input[type=\"password\"]"#)
                || html.contains("input[type=\"password\"]"),
            "the toggle should select every password input"
        );
        // The login form itself must still carry the password
        // field, otherwise the toggle has nothing to attach to.
        assert!(html.contains(r#"type="password""#));
    }

    #[test]
    fn render_home_landing_renders_paid_only_two_tier_pricing() {
        // The landing pricing collapsed from three tiers to two
        // when the product went paid-only: Standard (49.99 EUR /
        // seat, capped at 100 seats) and Enterprise (custom). The
        // Community $0 tier no longer exists.
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.contains("Standard"));
        assert!(html.contains("Enterprise"));
        assert!(
            !html.contains("Community"),
            "landing should NOT advertise a Community / free tier; the product is paid-only"
        );
        assert!(
            !html.contains("$0"),
            "landing should NOT advertise a $0 price point"
        );
        assert!(
            html.contains(r#"data-badge="Most popular""#),
            "Standard tier should carry the featured badge"
        );
        // The pricing subtitle must be unambiguous about the
        // software-only stance (no hosting, no compute resale).
        assert!(html.contains("software"));
        assert!(
            html.contains("never charge"),
            "pricing subtitle should explicitly disclaim usage / compute fees"
        );
    }

    #[test]
    fn render_home_landing_surfaces_the_four_trust_pillars() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        // Section + the four pillar titles must all be present.
        assert!(html.contains("Compliance evidence as a side-effect"));
        assert!(html.contains("Signed license envelopes"));
        assert!(html.contains("Encrypted secrets"));
        assert!(html.contains("Post-quantum readiness"));
        assert!(html.contains("EU AI Act deployer evidence"));
        // Each pillar links to the live admin page.
        assert!(html.contains("/admin/license"));
        assert!(html.contains("/admin/secrets"));
        assert!(html.contains("/admin/pq-status"));
        assert!(html.contains("/compliance/eu-ai-act"));
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
        let html = render_install_hub(&l, &[], &std::collections::HashSet::new());
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
        // E20: identity-and-access disclosure now carries real
        // form fields (IdP kind dropdown + discovery URL + etc.)
        // instead of the v0.1-placeholder banner.
        assert!(
            html.contains(r#"name="postgres__idp_kind""#),
            "every component card must expose an idp_kind selector"
        );
        assert!(
            html.contains(r#"name="postgres__idp_discovery_url""#),
            "every component card must expose an idp_discovery_url input"
        );
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
        let html = render_install_hub(&l, &active, &std::collections::HashSet::new());
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
        assert!(render_credentials_block(&l, &[], None).is_empty());
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
        let html = render_credentials_block(&l, &creds, None);
        assert!(html.contains("Generated credentials"));
        assert!(html.contains("Copy these values"));
        assert!(html.contains("postgres"));
        assert!(html.contains("deadbeef1234"));
        assert!(html.contains("kanidm"));
        assert!(html.contains("cafebabe5678"));
        assert!(html.contains("postgres/admin-password"));
    }

    #[test]
    fn render_credentials_block_includes_json_download_when_job_id_provided() {
        use computeza_driver_native::progress::GeneratedCredential;
        let l = Localizer::english();
        let creds = vec![GeneratedCredential {
            component: "postgres".into(),
            label: "superuser password".into(),
            value: "deadbeef1234".into(),
            username: Some("postgres".into()),
            secret_ref: Some("postgres/admin-password".into()),
        }];
        let html = render_credentials_block(&l, &creds, Some("job-xyz"));
        assert!(
            html.contains(r#"href="/install/credentials.json/job-xyz""#),
            "download button should target the JSON endpoint for the job"
        );
        assert!(html.contains("Download credentials as JSON"));
        assert!(html.contains("One-shot"));
    }

    #[test]
    fn render_credentials_block_omits_json_download_when_job_id_missing() {
        use computeza_driver_native::progress::GeneratedCredential;
        let l = Localizer::english();
        let creds = vec![GeneratedCredential {
            component: "postgres".into(),
            label: "superuser password".into(),
            value: "deadbeef1234".into(),
            username: Some("postgres".into()),
            secret_ref: Some("postgres/admin-password".into()),
        }];
        let html = render_credentials_block(&l, &creds, None);
        assert!(!html.contains("/install/credentials.json/"));
    }

    // ---- render_install_result_with_credentials ----

    #[test]
    fn install_result_with_credentials_omits_rollback_when_id_is_none() {
        let l = Localizer::english();
        let html = render_install_result_with_credentials(&l, true, "ok", &[], None, None);
        assert!(!html.contains("Roll back this install"));
    }

    #[test]
    fn install_result_with_credentials_renders_rollback_when_id_provided() {
        let l = Localizer::english();
        let html =
            render_install_result_with_credentials(&l, true, "ok", &[], Some("abc-123"), None);
        assert!(html.contains("Roll back this install"));
        assert!(html.contains(r#"action="/install/job/abc-123/rollback""#));
    }

    // ---- credentials_for_download one-shot drain semantics ----

    #[test]
    fn credentials_for_download_is_one_shot_drained() {
        // The handler relies on Option::take to enforce
        // view-once. Lock in that semantic without standing up
        // the whole axum router.
        use computeza_driver_native::progress::{GeneratedCredential, InstallProgress};
        let mut p = InstallProgress {
            credentials_for_download: Some(vec![GeneratedCredential {
                component: "postgres".into(),
                label: "superuser password".into(),
                value: "deadbeef1234".into(),
                username: Some("postgres".into()),
                secret_ref: Some("postgres/admin-password".into()),
            }]),
            ..Default::default()
        };
        let first = p.credentials_for_download.take();
        let second = p.credentials_for_download.take();
        assert!(
            first.is_some_and(|v| v.len() == 1),
            "first drain returns the bag"
        );
        assert!(
            second.is_none(),
            "second drain returns None (handler responds 410 Gone in this case)"
        );
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
            idp_config: None,
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
            idp_config: None,
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
            idp_config: None,
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
            idp_config: None,
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

    // ---- License enforcement ----

    fn dev_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    fn issue_test_license(
        not_before: chrono::DateTime<chrono::Utc>,
        not_after: chrono::DateTime<chrono::Utc>,
        seats: Option<u32>,
    ) -> computeza_license::License {
        let sk = dev_signing_key();
        let vk = sk.verifying_key();
        use base64ct::Encoding;
        let payload = computeza_license::LicensePayload {
            id: "test-license-0001".into(),
            tier: "standard".into(),
            seats,
            not_before,
            not_after,
            features: Vec::new(),
            billing_metadata: None,
            chain: vec![
                computeza_license::ChainEntry {
                    name: "Computeza Inc.".into(),
                    verifying_key: base64ct::Base64::encode_string(vk.as_bytes()),
                    pq_verifying_key: None,
                    support_contact: None,
                },
                computeza_license::ChainEntry {
                    name: "Test Customer".into(),
                    verifying_key: base64ct::Base64::encode_string(vk.as_bytes()),
                    pq_verifying_key: None,
                    support_contact: Some("ops@test.example".into()),
                },
            ],
        };
        computeza_license::issue(&sk, payload).expect("issue test license")
    }

    #[tokio::test]
    async fn license_status_none_in_community_mode() {
        let state = AppState::empty();
        assert!(matches!(
            state.license_status().await,
            computeza_license::LicenseStatus::None
        ));
    }

    #[tokio::test]
    async fn license_status_active_when_envelope_loaded() {
        let state = AppState::empty();
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(180),
            Some(100),
        );
        state.set_license(Some(lic)).await;
        let status = state.license_status().await;
        assert!(matches!(
            status,
            computeza_license::LicenseStatus::Active { .. }
        ));
        assert!(status.allow_mutations());
    }

    #[tokio::test]
    async fn license_status_expired_blocks_mutations() {
        let state = AppState::empty();
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(60),
            now - chrono::Duration::days(2),
            Some(50),
        );
        state.set_license(Some(lic)).await;
        let status = state.license_status().await;
        assert!(matches!(
            status,
            computeza_license::LicenseStatus::Expired { .. }
        ));
        assert!(!status.allow_mutations());
    }

    #[tokio::test]
    async fn api_license_status_endpoint_returns_json() {
        let state = AppState::empty();
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(15), // <30 = expiring-soon
            Some(10),
        );
        state.set_license(Some(lic)).await;
        let json = api_license_status_handler(State(state)).await.0;
        assert_eq!(json["kind"], "expiring-soon");
        assert_eq!(json["severity"], "warn");
        assert!(json["message"].as_str().unwrap().contains("expires in"));
    }

    #[tokio::test]
    async fn api_license_status_reports_active_when_well_within_window() {
        let state = AppState::empty();
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(180),
            None,
        );
        state.set_license(Some(lic)).await;
        let json = api_license_status_handler(State(state)).await.0;
        assert_eq!(json["kind"], "active");
        assert_eq!(json["severity"], "ok");
    }

    #[tokio::test]
    async fn api_license_status_reports_none_in_community_mode() {
        let state = AppState::empty();
        let json = api_license_status_handler(State(state)).await.0;
        assert_eq!(json["kind"], "none");
        assert_eq!(json["severity"], "ok");
    }

    #[test]
    fn render_admin_license_v2_shows_seat_usage_when_capped() {
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(365),
            Some(10),
        );
        let l = Localizer::english();
        let html = render_admin_license_v2(
            &l,
            Some(&lic),
            computeza_license::LicenseStatus::Active {
                days_remaining: 360,
            },
            Some(7),
            None,
        );
        assert!(
            html.contains("7 / 10"),
            "seat usage should render as used/cap"
        );
    }

    #[test]
    fn render_admin_license_v2_renders_billing_metadata() {
        let now = chrono::Utc::now();
        let sk = dev_signing_key();
        let vk = sk.verifying_key();
        use base64ct::Encoding;
        let payload = computeza_license::LicensePayload {
            id: "billing-test".into(),
            tier: "enterprise".into(),
            seats: None,
            not_before: now - chrono::Duration::days(1),
            not_after: now + chrono::Duration::days(365),
            features: vec!["ai-studio".into()],
            billing_metadata: Some(computeza_license::BillingNote {
                annual_value: Some(499_900),
                currency: Some("EUR".into()),
                contract_id: Some("ACME-2026-001".into()),
            }),
            chain: vec![
                computeza_license::ChainEntry {
                    name: "Computeza Inc.".into(),
                    verifying_key: base64ct::Base64::encode_string(vk.as_bytes()),
                    pq_verifying_key: None,
                    support_contact: None,
                },
                computeza_license::ChainEntry {
                    name: "Acme".into(),
                    verifying_key: base64ct::Base64::encode_string(vk.as_bytes()),
                    pq_verifying_key: None,
                    support_contact: None,
                },
            ],
        };
        let lic = computeza_license::issue(&sk, payload).unwrap();
        let l = Localizer::english();
        let html = render_admin_license_v2(
            &l,
            Some(&lic),
            computeza_license::LicenseStatus::Active {
                days_remaining: 360,
            },
            None,
            None,
        );
        assert!(html.contains("ACME-2026-001"));
        assert!(html.contains("EUR"));
        assert!(html.contains("4999.00"));
        assert!(html.contains("ai-studio"));
    }

    #[test]
    fn render_license_status_banner_silent_for_active_and_community() {
        assert_eq!(
            render_license_status_banner(&computeza_license::LicenseStatus::None),
            ""
        );
        assert_eq!(
            render_license_status_banner(&computeza_license::LicenseStatus::Active {
                days_remaining: 100
            }),
            ""
        );
    }

    #[test]
    fn render_license_status_banner_warns_when_expiring_soon() {
        let banner = render_license_status_banner(&computeza_license::LicenseStatus::Active {
            days_remaining: 14,
        });
        assert!(banner.contains("Renewal due"));
        assert!(banner.contains("14"));
    }

    // ---- Secrets first-boot wizard ----

    #[test]
    fn generate_passphrase_hex_returns_64_hex_chars() {
        let p = generate_passphrase_hex();
        assert_eq!(p.len(), 64);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_passphrase_hex_produces_distinct_values_per_call() {
        let a = generate_passphrase_hex();
        let b = generate_passphrase_hex();
        assert_ne!(a, b, "passphrases must be unique per call");
    }

    #[test]
    fn render_secrets_setup_generated_embeds_passphrase_and_systemd_template() {
        let l = Localizer::english();
        let pphr = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let html = render_secrets_setup_generated(&l, pphr);
        assert!(html.contains(pphr), "passphrase must render verbatim");
        assert!(html.contains("EnvironmentFile"));
        assert!(html.contains("computeza-secrets.salt"));
        assert!(html.contains("computeza-secrets.jsonl"));
        assert!(html.contains("daemon-reload"));
        assert!(html.contains("One-time view"));
    }

    #[test]
    fn render_secrets_index_shows_three_step_wizard_when_store_missing() {
        let l = Localizer::english();
        let html = render_secrets_index(&l, None);
        assert!(html.contains("Step 1"));
        assert!(html.contains("Step 2"));
        assert!(html.contains("Step 3"));
        assert!(html.contains("/admin/secrets/setup/generate-passphrase"));
    }

    // ---- Operator group checkbox UI ----

    #[test]
    fn group_checkboxes_html_renders_one_box_per_builtin_group() {
        let html = group_checkboxes_html("new", &[]);
        // One checkbox per BUILTIN_GROUPS entry.
        for (name, _) in auth::BUILTIN_GROUPS {
            assert!(
                html.contains(&format!(r#"value="{name}""#)),
                "expected checkbox for group {name}"
            );
        }
        // No checkbox starts pre-checked when `current` is empty.
        assert!(!html.contains(" checked"));
    }

    #[test]
    fn group_checkboxes_html_marks_current_groups_checked() {
        let html = group_checkboxes_html("row", &["operators".into(), "viewers".into()]);
        assert!(
            html.contains(r#"value="operators" checked"#),
            "operators must be pre-checked"
        );
        assert!(
            html.contains(r#"value="viewers" checked"#),
            "viewers must be pre-checked"
        );
        assert!(
            !html.contains(r#"value="admins" checked"#),
            "admins must not be pre-checked"
        );
    }

    #[test]
    fn render_admin_operators_emits_checkboxes_not_free_text_input() {
        let l = Localizer::english();
        let now = chrono::Utc::now();
        let op = auth::OperatorRecord {
            username: "alice".into(),
            password_hash: "x".into(),
            groups: vec!["admins".into()],
            created_at: now,
        };
        let html = render_admin_operators(&l, std::slice::from_ref(&op), "alice", None);
        // No free-text "groups" input anywhere.
        assert!(
            !html.contains(r#"<input type="text" name="groups""#),
            "free-text groups input must be replaced with checkboxes"
        );
        assert!(
            !html.contains(r#"name="groups" class="cz-input" type="text""#),
            "create-form free-text groups input must be replaced"
        );
        // Three checkboxes per row + three for the create form = 6.
        let checkbox_count = html.matches(r#"type="checkbox" name="groups""#).count();
        assert_eq!(
            checkbox_count, 6,
            "expected 3 checkboxes per row + 3 in create form (one row in this test)"
        );
    }

    // ---- EU AI Act compliance ----

    #[test]
    fn render_compliance_eu_ai_act_lists_article_checklist_and_marketing_caveat() {
        let l = Localizer::english();
        let html = render_compliance_eu_ai_act(&l);
        assert!(html.contains("EU AI Act compliance"));
        assert!(html.contains("Art 9"));
        assert!(html.contains("Art 50"));
        // Marketing-language caveat is in the body.
        assert!(html.contains("tooling vendor"));
        // Cross-links to the model card registry.
        assert!(html.contains("/compliance/models"));
    }

    #[test]
    fn render_compliance_models_list_explains_smoke_test_when_registry_missing() {
        let l = Localizer::english();
        let html = render_compliance_models_list(&l, &[], false, None);
        assert!(html.contains("No model-card registry"));
    }

    #[test]
    fn render_compliance_models_list_renders_create_form_when_attached() {
        let l = Localizer::english();
        let html = render_compliance_models_list(&l, &[], true, None);
        assert!(html.contains("Register a model card"));
        assert!(html.contains(r#"name="risk""#));
        assert!(html.contains("high-risk"));
        assert!(html.contains("limited-risk"));
        assert!(html.contains("minimal"));
        // Prohibited is NOT a registerable option.
        assert!(!html.contains(r#"<option value="prohibited">"#));
    }

    #[test]
    fn render_compliance_models_list_renders_row_per_card() {
        let l = Localizer::english();
        let now = chrono::Utc::now();
        let card = computeza_compliance::ModelCard {
            id: "credit-risk-v3".into(),
            name: "Credit risk v3".into(),
            risk: computeza_compliance::RiskClassification::HighRisk,
            risk_justification: computeza_compliance::RiskJustification {
                rationale: "Annex III item 5(b).".into(),
                citations: Vec::new(),
            },
            intended_use: "Score loans.".into(),
            training_data_summary: "Internal data.".into(),
            limitations: "Limits.".into(),
            human_oversight_design: "Underwriter override.".into(),
            evaluation_metrics: Vec::new(),
            deployments: Vec::new(),
            article_evidence: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let html = render_compliance_models_list(&l, std::slice::from_ref(&card), true, None);
        assert!(html.contains("credit-risk-v3"));
        assert!(html.contains("Credit risk v3"));
        assert!(html.contains("High-risk"));
        // High-risk with no evidence yet => "incomplete" badge.
        assert!(html.contains("Art 9-15 incomplete"));
    }

    #[test]
    fn render_compliance_model_detail_renders_checklist_for_high_risk() {
        let l = Localizer::english();
        let now = chrono::Utc::now();
        let card = computeza_compliance::ModelCard {
            id: "bio-screen".into(),
            name: "Biometric screening".into(),
            risk: computeza_compliance::RiskClassification::HighRisk,
            risk_justification: computeza_compliance::RiskJustification {
                rationale: "Annex III item 1.".into(),
                citations: Vec::new(),
            },
            intended_use: "Border screening assist.".into(),
            training_data_summary: "Public benchmark.".into(),
            limitations: "Not for autonomous decisions.".into(),
            human_oversight_design: "Officer reviews every match.".into(),
            evaluation_metrics: Vec::new(),
            deployments: Vec::new(),
            article_evidence: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let html = render_compliance_model_detail(&l, &card);
        assert!(html.contains("Article 9-15 evidence checklist"));
        assert!(html.contains("Art 9"));
        assert!(html.contains("Art 15"));
    }

    #[test]
    fn render_admin_pq_status_reports_hybrid_kex_and_community_mode() {
        let l = Localizer::english();
        let pq = computeza_channel_partner::pq::tls_readiness();
        let html = render_admin_pq_status(&l, &pq, None);
        assert!(html.contains("Post-quantum readiness"));
        assert!(html.contains("X25519MLKEM768"));
        assert!(html.contains("aws-lc-rs"));
        assert!(html.contains("Community mode"));
    }

    #[test]
    fn render_admin_pq_status_renders_license_section_when_active() {
        let l = Localizer::english();
        let now = chrono::Utc::now();
        let lic = issue_test_license(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(180),
            Some(50),
        );
        let pq = computeza_channel_partner::pq::tls_readiness();
        let html = render_admin_pq_status(&l, &pq, Some(&lic));
        assert!(html.contains(&lic.payload.id));
        assert!(html.contains("Dual-sig envelope shape supported"));
    }

    #[test]
    fn render_admin_license_v2_links_to_pq_status_dashboard() {
        let l = Localizer::english();
        let html =
            render_admin_license_v2(&l, None, computeza_license::LicenseStatus::None, None, None);
        assert!(html.contains("/admin/pq-status"));
    }

    #[test]
    fn render_license_status_banner_flags_expired() {
        let banner = render_license_status_banner(&computeza_license::LicenseStatus::Expired {
            days_since_expiry: 3,
        });
        assert!(banner.contains("expired"));
        assert!(banner.contains("3"));
    }
}
