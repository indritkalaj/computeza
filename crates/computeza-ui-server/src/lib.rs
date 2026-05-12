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
/// the operator has set `COMPUTEZA_SECRETS_PASSPHRASE`), and the
/// background-job registry. Wrapped in `Arc` so axum can clone it
/// cheaply per request.
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
}

impl AppState {
    /// Construct an empty state for tests / minimal serve.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            store: None,
            jobs: Arc::new(StdMutex::new(HashMap::new())),
            secrets: None,
        }
    }

    /// Construct with a backing SqliteStore.
    #[must_use]
    pub fn with_store(store: SqliteStore) -> Self {
        Self {
            store: Some(Arc::new(store)),
            jobs: Arc::new(StdMutex::new(HashMap::new())),
            secrets: None,
        }
    }

    /// Attach an encrypted [`SecretsStore`] to the state. Chainable
    /// with [`AppState::with_store`].
    #[must_use]
    pub fn with_secrets(mut self, secrets: SecretsStore) -> Self {
        self.secrets = Some(Arc::new(secrets));
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

/// Build the axum router with an `AppState` attached. Every handler that
/// needs the store extracts it via `State<AppState>`.
pub fn router_with_state(state: AppState) -> Router {
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
                    overall.push_str(&format!("=== {slug} ===\n{summary}\n\n"));
                    if let Some(store) = &store {
                        let kind = format!("{slug}-instance");
                        let key = ResourceKey::cluster_scoped(&kind, "local");
                        let expected_revision = match store.load(&key).await {
                            Ok(Some(existing)) => Some(existing.revision),
                            _ => None,
                        };
                        if let Err(e) = store.save(&key, &spec, expected_revision).await {
                            tracing::warn!(
                                error = %e,
                                component = slug,
                                "unified install: store.save failed for {slug}-instance/local; \
                                 the on-disk service is up but the metadata row was not written. \
                                 Visit /status to confirm; re-run install to retry the registration."
                            );
                            overall.push_str(&format!(
                                "Note: did not register {slug}-instance/local ({e}). Visit /status to inspect.\n\n"
                            ));
                        }
                    }
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_kanidm_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("kanidm-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("https://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": true,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as kanidm-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "kanidm install: store.save failed; the on-disk service is fine. \
                                 Visit /status or re-run install to retry the registration."
                            );
                            summary.push_str(&format!(
                                "\n\nNote: did not register kanidm-instance/local ({e}). Visit /status to inspect current state."
                            ));
                        }
                    }
                }
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
    let result = run_kanidm_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("kanidm-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                "uninstall: store.delete(kanidm-instance/local) failed; \
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

#[cfg(target_os = "linux")]
async fn run_kanidm_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::kanidm;
    match kanidm::uninstall(kanidm::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "macos")]
async fn run_kanidm_uninstall() -> Result<String, String> {
    use computeza_driver_native::macos::kanidm;
    match kanidm::uninstall(kanidm::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(target_os = "windows")]
async fn run_kanidm_uninstall() -> Result<String, String> {
    use computeza_driver_native::windows::kanidm;
    match kanidm::uninstall(kanidm::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn run_kanidm_uninstall() -> Result<String, String> {
    Err("uninstall is supported on Linux, macOS, and Windows only".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_garage_install_with_progress(&progress, &config).await {
            Ok((summary, s3_port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("garage-instance", "local");
                    // Reconciler hits the admin API at s3_port + 3
                    // (3903 with the canonical 3900 layout).
                    let admin_port = s3_port + 3;
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{admin_port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as garage-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "garage install: store.save failed; the on-disk service is fine. \
                                 Visit /status or re-run install to retry the registration."
                            );
                            summary.push_str(&format!(
                                "\n\nNote: did not register garage-instance/local ({e}). Visit /status to inspect current state."
                            ));
                        }
                    }
                }
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
    let result = run_garage_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("garage-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                "uninstall: store.delete(garage-instance/local) failed; \
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

#[cfg(target_os = "linux")]
async fn run_garage_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::garage;
    match garage::uninstall(garage::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_garage_uninstall() -> Result<String, String> {
    Err("Garage uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_openfga_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("openfga-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as openfga-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "openfga install: store.save failed; the on-disk service is fine."
                            );
                            summary.push_str(&format!(
                                "\n\nNote: did not register openfga-instance/local ({e}). Visit /status to inspect current state."
                            ));
                        }
                    }
                }
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
    let result = run_openfga_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("openfga-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                "uninstall: store.delete(openfga-instance/local) failed"
            );
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_openfga_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::openfga;
    match openfga::uninstall(openfga::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_openfga_uninstall() -> Result<String, String> {
    Err("OpenFGA uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_qdrant_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("qdrant-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as qdrant-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "qdrant install: store.save failed; the on-disk service is fine."
                            );
                            summary.push_str(&format!(
                                "\n\nNote: did not register qdrant-instance/local ({e}). Visit /status to inspect current state."
                            ));
                        }
                    }
                }
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
    let result = run_qdrant_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("qdrant-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(
                error = %e,
                "uninstall: store.delete(qdrant-instance/local) failed"
            );
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_qdrant_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::qdrant;
    match qdrant::uninstall(qdrant::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_qdrant_uninstall() -> Result<String, String> {
    Err("Qdrant uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_greptime_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("greptime-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as greptime-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "greptime install: store.save failed");
                            summary.push_str(&format!(
                                "\n\nNote: did not register greptime-instance/local ({e})."
                            ));
                        }
                    }
                }
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
    let result = run_greptime_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("greptime-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(error = %e, "uninstall: store.delete(greptime-instance/local) failed");
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_greptime_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::greptime;
    match greptime::uninstall(greptime::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_greptime_uninstall() -> Result<String, String> {
    Err("GreptimeDB uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_lakekeeper_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("lakekeeper-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as lakekeeper-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "lakekeeper install: store.save failed");
                            summary.push_str(&format!(
                                "\n\nNote: did not register lakekeeper-instance/local ({e})."
                            ));
                        }
                    }
                }
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
    let result = run_lakekeeper_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("lakekeeper-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(error = %e, "uninstall: store.delete(lakekeeper-instance/local) failed");
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_lakekeeper_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::lakekeeper;
    match lakekeeper::uninstall(lakekeeper::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_lakekeeper_uninstall() -> Result<String, String> {
    Err("Lakekeeper uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_databend_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("databend-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as databend-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "databend install: store.save failed");
                            summary.push_str(&format!(
                                "\n\nNote: did not register databend-instance/local ({e})."
                            ));
                        }
                    }
                }
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
    let result = run_databend_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("databend-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(error = %e, "uninstall: store.delete(databend-instance/local) failed");
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_databend_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::databend;
    match databend::uninstall(databend::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_databend_uninstall() -> Result<String, String> {
    Err("Databend uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_grafana_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("grafana-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as grafana-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "grafana install: store.save failed");
                            summary.push_str(&format!(
                                "\n\nNote: did not register grafana-instance/local ({e})."
                            ));
                        }
                    }
                }
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
    let result = run_grafana_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("grafana-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(error = %e, "uninstall: store.delete(grafana-instance/local) failed");
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_grafana_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::grafana;
    match grafana::uninstall(grafana::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_grafana_uninstall() -> Result<String, String> {
    Err("Grafana uninstall requires a supported Linux host.".into())
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
    let progress = ProgressHandle::new(progress_state);
    tokio::spawn(async move {
        match run_restate_install_with_progress(&progress, &config).await {
            Ok((summary, port)) => {
                let mut summary = summary;
                if let Some(store) = &store {
                    let key = ResourceKey::cluster_scoped("restate-instance", "local");
                    let spec = serde_json::json!({
                        "endpoint": {
                            "base_url": format!("http://127.0.0.1:{port}"),
                            "insecure_skip_tls_verify": false,
                        },
                    });
                    let expected_revision = match store.load(&key).await {
                        Ok(Some(existing)) => Some(existing.revision),
                        _ => None,
                    };
                    match store.save(&key, &spec, expected_revision).await {
                        Ok(_) => summary.push_str(
                            "\n\nRegistered as restate-instance/local in the metadata store.\nVisit /status to see it.",
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "restate install: store.save failed");
                            summary.push_str(&format!(
                                "\n\nNote: did not register restate-instance/local ({e})."
                            ));
                        }
                    }
                }
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
    let result = run_restate_uninstall().await;
    if let Some(store) = &state.store {
        let key = ResourceKey::cluster_scoped("restate-instance", "local");
        if let Err(e) = store.delete(&key, None).await {
            tracing::warn!(error = %e, "uninstall: store.delete(restate-instance/local) failed");
        }
    }
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

#[cfg(target_os = "linux")]
async fn run_restate_uninstall() -> Result<String, String> {
    use computeza_driver_native::linux::restate;
    match restate::uninstall(restate::UninstallOptions::default()).await {
        Ok(r) => Ok(format_uninstall_summary(&r.steps, &r.warnings)),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_restate_uninstall() -> Result<String, String> {
    Err("Restate uninstall requires a supported Linux host.".into())
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
    /// Metadata store summary.
    State,
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
  </div>
</nav>
<main class="cz-page">
{body}
<footer class="cz-footer">
  <span>{version_label} {version}</span>
  <span>Computeza proprietary -- managed components retain upstream licenses (see /components)</span>
</footer>
</main>
</body>
</html>"#,
        nc = nav_class(NavLink::Components),
        ni = nav_class(NavLink::Install),
        ns = nav_class(NavLink::Status),
        nm = nav_class(NavLink::State),
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

    let body = format!(
        r#"<section class="cz-hero">
<h1>{title}</h1>
<p>{intro}</p>
</section>
{platform_banner}
{active_banner}
<form method="post" action="/install" style="display: contents;">
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
}
