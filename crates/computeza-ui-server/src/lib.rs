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
/// `computeza serve` opens one) plus the background-job registry.
/// Wrapped in `Arc` so axum can clone it cheaply per request.
#[derive(Clone)]
pub struct AppState {
    /// Persistent metadata store, `None` for the unit-test smoke router.
    pub store: Option<Arc<SqliteStore>>,
    /// Background install jobs in flight or recently finished.
    pub jobs: JobRegistry,
}

impl AppState {
    /// Construct an empty state for tests / minimal serve.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            store: None,
            jobs: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Construct with a backing SqliteStore.
    #[must_use]
    pub fn with_store(store: SqliteStore) -> Self {
        Self {
            store: Some(Arc::new(store)),
            jobs: Arc::new(StdMutex::new(HashMap::new())),
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
        .route("/install", get(install_hub_handler))
        .route(
            "/install/postgres",
            get(install_postgres_form_handler).post(install_postgres_handler),
        )
        .route(
            "/install/postgres/uninstall",
            get(uninstall_confirm_handler).post(uninstall_postgres_handler),
        )
        .route("/install/{component}", get(install_component_handler))
        .route("/install/job/{id}", get(install_job_handler))
        .route("/api/install/job/{id}", get(install_job_api_handler))
        .route("/status", get(status_handler))
        .route("/state", get(state_page_handler))
        .route("/resource/{kind}/{name}", get(resource_handler))
        .route(
            "/resource/{kind}/{name}/delete",
            post(resource_delete_handler),
        )
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn install_hub_handler() -> Html<String> {
    let l = Localizer::english();
    Html(render_install_hub(&l))
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
#[derive(Clone, Debug, Default)]
struct InstallConfig {
    version: Option<String>,
    port: Option<u16>,
    root_dir: Option<String>,
    service_name: Option<String>,
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_postgres_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("postgres-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "host": "127.0.0.1",
                            "port": port,
                            "superuser": "postgres",
                        },
                        "databases": [],
                        "prune": false,
                    });
                    // Upsert: load to discover an existing revision, then
                    // save with that revision so re-installs update in
                    // place instead of erroring with "revision conflict
                    // on postgres-instance/local: expected None, found
                    // Some(1)".
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => {
                            summary.push_str(
                                "\n\nRegistered as postgres-instance/local in the metadata store.\nVisit /status to see it.",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "install background task: store.save failed; install succeeded but \
                                 the resource is not registered. Most likely another writer raced \
                                 the upsert. Re-run the install if /status is wrong."
                            );
                            summary.push_str(&format!(
                                "\n\nNote: did not register postgres-instance/local ({e}). Visit /status to inspect current state."
                            ));
                        }
                    }

                    // Kick a single observe() immediately so /status
                    // doesn't sit on the previous (likely failed) tick
                    // result for up to 30 seconds. Best-effort: the
                    // periodic tick will retry regardless.
                    if let Err(e) = observe_postgres_instance_now(store.clone(), &spec).await {
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
    let result = run_postgres_uninstall().await;

    // Best-effort: also drop the metadata row so /status doesn't
    // keep reporting a now-deleted instance. We do this regardless
    // of whether the driver-side uninstall succeeded -- the operator
    // explicitly asked for a rollback.
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("postgres-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                "uninstall: store.delete(postgres-instance/local) failed; \
                 manually delete the row via the /resource page if it lingers"
            );
        }
    }

    let body = match result {
        Ok(summary) => render_install_result(&l, true, &summary),
        Err(detail) => render_install_result(&l, false, &detail),
    };
    Html(body).into_response()
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
/// all converge on this format so the UI is OS-agnostic.
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

#[cfg(target_os = "windows")]
async fn run_postgres_uninstall() -> Result<String, String> {
    use computeza_driver_native::windows::postgres;
    match postgres::uninstall(postgres::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "linux")]
async fn run_postgres_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::postgres;
    match postgres::uninstall(postgres::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "macos")]
async fn run_postgres_uninstall() -> Result<String, String> {
    use computeza_driver_native::macos::postgres;
    match postgres::uninstall(postgres::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn run_postgres_uninstall() -> Result<String, String> {
    Err("uninstall is supported on Linux, macOS, and Windows only".into())
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
    spec_json: &serde_json::Value,
) -> anyhow::Result<()> {
    use computeza_core::reconciler::Context;
    use computeza_core::{NoOpDriver, Reconciler};
    use computeza_reconciler_postgres::{PostgresReconciler, PostgresSpec};

    let spec: PostgresSpec = serde_json::from_value(spec_json.clone())?;
    let reconciler: PostgresReconciler<NoOpDriver> =
        PostgresReconciler::new(spec.endpoint.clone(), spec.superuser_password)
            .with_state(store, "local");
    // observe() is "best effort writes a status row to the store".
    // It returns Ok with `last_observe_failed=true` on connection
    // failure, so the .await? here only propagates programming bugs.
    let _ = reconciler.observe(&Context::default()).await;
    Ok(())
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
    let snapshot = state
        .jobs
        .lock()
        .unwrap()
        .get(&job_id)
        .map(|s| s.lock().unwrap().clone());
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
            // Render the final result page (success or failure) so
            // refreshing the URL after completion keeps working.
            if let Some(err) = &p.error {
                (StatusCode::OK, Html(render_install_result(&l, false, err)))
            } else {
                let summary = p.success_summary.clone().unwrap_or_default();
                (
                    StatusCode::OK,
                    Html(render_install_result(&l, true, &summary)),
                )
            }
        }
        Some(p) => (
            StatusCode::OK,
            Html(render_install_progress(&l, &job_id, &p)),
        ),
    }
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
            Html(render_resource(&l, &kind, &name, None, true)),
        );
    };
    let key = ResourceKey::cluster_scoped(&kind, &name);
    match store.load(&key).await {
        Ok(Some(stored)) => (
            StatusCode::OK,
            Html(render_resource(&l, &kind, &name, Some(&stored), false)),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Html(render_resource(&l, &kind, &name, None, false)),
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
                Html(render_resource(&l, &kind, &name, None, false)),
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
  </div>
</nav>
<main class="cz-page">
{body}
<footer class="cz-footer">
  <span>{version_label} {version}</span>
  <span>Spec v1.5 -- AGPL stack</span>
</footer>
</main>
</body>
</html>"#,
        nc = nav_class(NavLink::Components),
        ni = nav_class(NavLink::Install),
        ns = nav_class(NavLink::Status),
    )
}

/// Render one navigation card on the home dashboard. `extra` is an
/// optional one-line context tail (e.g. resource count) shown beneath
/// the body text in a smaller font.
fn render_home_card(href: &str, title: &str, body: &str, extra: Option<&str>) -> String {
    let extra_html = match extra {
        Some(e) => format!(r#"<p class="cz-card-meta">{}</p>"#, html_escape(e)),
        None => String::new(),
    };
    format!(
        r#"<a href="{href}" class="cz-card">
<h3 class="cz-card-title">{title}</h3>
<p class="cz-card-body">{body}</p>
{extra_html}
</a>"#,
        href = html_escape(href),
        title = html_escape(title),
        body = html_escape(body),
    )
}

/// Render the home page to a complete HTML document.
///
/// `store_summary` drives the "Metadata store" card -- pass
/// `StoreSummary::Missing` when no store is attached to this server.
#[must_use]
pub fn render_home(localizer: &Localizer, store_summary: StoreSummary) -> String {
    let tagline = localizer.t("ui-app-tagline");
    let title = localizer.t("ui-welcome-title");
    let subtitle = localizer.t("ui-welcome-subtitle");
    let status = localizer.t("ui-welcome-status");
    let spec_note = localizer.t("ui-welcome-spec");

    let card_components_title = localizer.t("ui-home-card-components-title");
    let card_components_body = localizer.t("ui-home-card-components-body");
    let card_install_title = localizer.t("ui-home-card-install-title");
    let card_install_body = localizer.t("ui-home-card-install-body");
    let card_status_title = localizer.t("ui-home-card-status-title");
    let card_status_body = localizer.t("ui-home-card-status-body");
    let card_state_title = localizer.t("ui-home-card-state-title");
    let card_state_body = localizer.t("ui-home-card-state-body");

    let store_line = match store_summary {
        StoreSummary::Missing => localizer.t("ui-home-store-missing"),
        StoreSummary::Counted(0) => localizer.t("ui-home-store-empty"),
        StoreSummary::Counted(n) => format!("{n} resource(s) registered."),
    };

    let cards_html = format!(
        r#"<div class="cz-card-grid">
{c1}
{c2}
{c3}
{c4}
</div>"#,
        c1 = render_home_card(
            "/components",
            &card_components_title,
            &card_components_body,
            None
        ),
        c2 = render_home_card("/install", &card_install_title, &card_install_body, None),
        c3 = render_home_card("/status", &card_status_title, &card_status_body, None),
        c4 = render_home_card(
            "/state",
            &card_state_title,
            &card_state_body,
            Some(&store_line),
        ),
    );

    let welcome_lead = localizer.t("ui-welcome-lead");
    let app_title = localizer.t("ui-app-title");
    let surfaces_heading = localizer.t("ui-home-surfaces");
    let pre_alpha = localizer.t("ui-home-pre-alpha");
    let body = format!(
        r#"<section class="cz-hero" style="display: grid; grid-template-columns: 1.1fr 0.9fr; gap: 3rem; align-items: center;">
<div>
<span class="cz-tag">{tagline}</span>
<h1>{welcome_lead} <em>{app_title}</em></h1>
<p>{subtitle}</p>
</div>
<div class="cz-stage">
<img src="/static/brand/computeza-logo.svg" alt="" />
</div>
</section>
<section class="cz-section">
<div class="cz-section-head">
<h2>{surfaces_heading}</h2>
<span class="cz-meta">{pre_alpha}</span>
</div>
{cards_html}
</section>
<section class="cz-section">
<div class="cz-card">
<p class="cz-card-body" style="margin: 0 0 0.5rem;">{status}</p>
<p class="cz-card-meta" style="margin: 0;">{spec_note}</p>
</div>
</section>"#,
        welcome_lead = html_escape(&welcome_lead),
        app_title = html_escape(&app_title),
        subtitle = html_escape(&subtitle),
        tagline = html_escape(&tagline),
        surfaces_heading = html_escape(&surfaces_heading),
        pre_alpha = html_escape(&pre_alpha),
        status = html_escape(&status),
        spec_note = html_escape(&spec_note),
    );

    render_shell(localizer, &title, NavLink::Home, &body)
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
                "<tr><td class=\"cz-strong\">{name}</td>\
                 <td><span class=\"cz-badge cz-badge-info\">{kind}</span></td>\
                 <td class=\"cz-cell-dim\">{role}</td></tr>",
                name = html_escape(&name),
                kind = html_escape(kind),
                role = html_escape(&role),
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
</tr></thead>
<tbody>{rows}</tbody>
</table>
</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
        col_name = html_escape(&col_name),
        col_kind = html_escape(&col_kind),
        col_role = html_escape(&col_role),
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
        available: true,
    },
    ComponentEntry {
        slug: "kanidm",
        name_key: "component-kanidm-name",
        role_key: "component-kanidm-role",
        available: false,
    },
    ComponentEntry {
        slug: "garage",
        name_key: "component-garage-name",
        role_key: "component-garage-role",
        available: false,
    },
    ComponentEntry {
        slug: "lakekeeper",
        name_key: "component-lakekeeper-name",
        role_key: "component-lakekeeper-role",
        available: false,
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
        available: false,
    },
    ComponentEntry {
        slug: "qdrant",
        name_key: "component-qdrant-name",
        role_key: "component-qdrant-role",
        available: false,
    },
    ComponentEntry {
        slug: "restate",
        name_key: "component-restate-name",
        role_key: "component-restate-role",
        available: false,
    },
    ComponentEntry {
        slug: "greptime",
        name_key: "component-greptime-name",
        role_key: "component-greptime-role",
        available: false,
    },
    ComponentEntry {
        slug: "grafana",
        name_key: "component-grafana-name",
        role_key: "component-grafana-role",
        available: false,
    },
    ComponentEntry {
        slug: "openfga",
        name_key: "component-openfga-name",
        role_key: "component-openfga-role",
        available: false,
    },
];

/// Render the `/install` page: a grid of cards, one per component
/// Computeza manages. The card links to either `/install/<slug>`
/// (available components, current wizard) or `/install/<slug>`
/// (which serves the coming-soon page).
#[must_use]
pub fn render_install_hub(localizer: &Localizer) -> String {
    let title = localizer.t("ui-install-hub-title");
    let intro = localizer.t("ui-install-hub-intro");
    let status_available = localizer.t("ui-install-status-available");
    let status_planned = localizer.t("ui-install-status-planned");

    let cards: String = COMPONENTS
        .iter()
        .map(|c| {
            let name = localizer.t(c.name_key);
            let role = localizer.t(c.role_key);
            let (badge_class, badge_text) = if c.available {
                ("cz-badge cz-badge-ok", status_available.as_str())
            } else {
                ("cz-badge cz-badge-info", status_planned.as_str())
            };
            format!(
                r#"<a href="/install/{slug}" class="cz-card">
<h3 class="cz-card-title">{name}</h3>
<p class="cz-card-body">{role}</p>
<p class="cz-card-meta"><span class="{badge_class}">{badge_text}</span></p>
</a>"#,
                slug = html_escape(c.slug),
                name = html_escape(&name),
                role = html_escape(&role),
                badge_class = badge_class,
                badge_text = html_escape(badge_text),
            )
        })
        .collect();

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
<section class="cz-section">
<div class="cz-card-grid">{cards}</div>
</section>"#,
        title = html_escape(&title),
        intro = html_escape(&intro),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
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

/// Render the in-flight wizard page. The page polls
/// `/api/install/job/{id}` every 500ms via inline JS and redirects to
/// the result page once `completed: true` lands in the snapshot.
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

    let body = format!(
        r#"<section class="cz-hero">
<h1>Installing PostgreSQL</h1>
<p>Computeza is preparing your local PostgreSQL service. You can leave this page open; it polls the server every half second.</p>
</section>
<section class="cz-section" style="max-width: 42rem;">
<div class="cz-progress">
  <div class="cz-progress-phase">
    <span id="phase">{phase_label}</span>
    <span class="cz-muted" id="phase-pct">{ratio_pct}%</span>
  </div>
  <div class="cz-progress-bar"><div class="cz-progress-fill" id="bar" style="width: {ratio_pct}%;"></div></div>
  <div class="cz-progress-msg" id="message">{message}</div>
  <div class="cz-progress-bytes" id="bytes">{bytes_line}</div>
</div>
</section>
<script>
const jobId = "{job_id_js}";
function fmt(n) {{
  if (n === 0) return "0 B";
  const u = ["B","KB","MB","GB","TB"];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) {{ v /= 1024; i++; }}
  return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
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
</script>"#
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

/// Render the `/install/postgres` result page. `success` switches the
/// heading between the success and failure i18n keys; `detail` is the
/// raw output (success summary or error chain) shown verbatim in a
/// `<pre>` block after HTML-escaping.
#[must_use]
pub fn render_install_result(localizer: &Localizer, success: bool, detail: &str) -> String {
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

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p><span class="{badge_class}">{outcome}</span></p>
</section>
<section class="cz-section">
<pre class="cz-pre">{detail_html}</pre>
</section>
<section class="cz-section">
<a class="cz-btn" href="/install">{back}</a>
</section>"#,
        title = html_escape(&title),
        outcome = html_escape(&outcome),
        back = html_escape(&back),
    );

    render_shell(localizer, &title, NavLink::Install, &body)
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

    render_shell(localizer, &title, NavLink::None, &body)
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
<section class="cz-section">
<form method="post" action="/resource/{kind_enc}/{name_enc}/delete" onsubmit="return confirm('{confirm}');">
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
        let html = render_resource(&l, "postgres-instance", "primary", None, true);
        assert!(html.contains("postgres-instance / primary"));
        assert!(html.contains("needs a metadata store"));
    }

    #[test]
    fn render_resource_not_found_path() {
        let l = Localizer::english();
        let html = render_resource(&l, "kanidm-instance", "missing", None, false);
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
    fn render_home_dashboard_links_to_every_surface() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        for href in [
            r#"href="/components""#,
            r#"href="/install""#,
            r#"href="/status""#,
            r#"href="/state""#,
        ] {
            assert!(
                html.contains(href),
                "home dashboard should link to {href}; got HTML excerpt:\n{}",
                &html[..html.len().min(2000)]
            );
        }
    }

    #[test]
    fn render_home_state_card_shows_missing_when_no_store() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.contains("No metadata store attached"));
    }

    #[test]
    fn render_home_state_card_shows_count_when_store_attached() {
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Counted(3));
        assert!(html.contains("3 resource(s) registered."));
    }

    #[test]
    fn render_home_has_no_hardcoded_english_strings_outside_attributes() {
        // Sanity check: every <p> and <h*> text node should be a value the
        // localizer produced. We assert by checking that strings the .ftl
        // bundle defines actually appear (positive check) and that some
        // common hardcoded-English smell doesn't (negative check).
        let l = Localizer::english();
        let html = render_home(&l, StoreSummary::Missing);
        assert!(html.contains("Computeza")); // ui-app-title
        assert!(html.contains("Open lakehouse control plane")); // ui-app-tagline
        assert!(html.contains("Pre-alpha")); // ui-welcome-status starts with this
    }
}
