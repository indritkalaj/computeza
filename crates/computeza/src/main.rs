//! Computeza -- single binary entry point.
//!
//! Per spec section 2.1, this binary is three things:
//!
//! 1. **Installer** -- first-run wizard that lays down the entire data plane
//!    natively on the host operating system (`computeza install`).
//! 2. **Operator console** -- long-running server hosting the Leptos web UI,
//!    REST API, gRPC API, and the reconciliation loop (`computeza serve`).
//! 3. **Configurator** -- declarative state engine with a YAML representation
//!    suitable for git, CI, and IaC workflows (`computeza apply`, etc.).
//!
//! Every command surface here is GUI-equivalent: anything you can do from
//! this CLI you can also do from the web console (and vice versa). The CLI
//! exists for power users, scripting, and CI; the web console is the
//! primary interface (spec section 2.1, section 4.2, and the user mandate that
//! everything -- instances, clusters, users, permissions -- be reachable
//! from a GUI).
//!
//! User-facing strings route through [`computeza_i18n::Localizer`]. Clap's
//! derive macros require `&'static str` for help text, which is a known
//! limitation; the `Cli` struct below uses `command_help_*` keys as
//! placeholders that will be replaced by builder-pattern construction
//! reading from the localizer once the command surface stabilizes.

#![warn(missing_docs)]

use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use computeza_i18n::Localizer;

/// Computeza command-line entry point.
#[derive(Parser, Debug)]
#[command(
    name = "computeza",
    version,
    // about/long_about left unset so clap renders the short binary description;
    // localised banner is printed when no subcommand is given (see main()).
    disable_help_subcommand = true,
)]
struct Cli {
    /// Subcommand to run; if omitted, prints the welcome banner.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level command surface. Each variant maps 1:1 to a section of the
/// web console (spec section 4.2).
#[derive(Subcommand, Debug)]
enum Command {
    /// First-run installer.
    Install {
        /// Component to install. v0.0.x ships only `postgres`.
        component: InstallComponent,
    },
    /// Start the operator console (web UI + reconciler loop).
    Serve {
        /// Address to bind the HTTP server to. Default `127.0.0.1:8400`
        /// per the port allocation in spec section 10.6.
        #[arg(long, default_value = "127.0.0.1:8400")]
        addr: SocketAddr,

        /// Path to the SQLite metadata store file. Default
        /// `./computeza-state.db` (relative to CWD). In production the
        /// installer drops this under /var/lib/computeza on Linux,
        /// /Library/Application Support/Computeza on macOS, and
        /// %PROGRAMDATA%\Computeza on Windows; the flag lets dev / test
        /// invocations point at a writable path without root.
        #[arg(long, default_value = "computeza-state.db")]
        state_db: String,
    },
    /// Show cluster status and reconciliation drift.
    Status {
        /// Address of the running operator console to probe.
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: String,
    },
    /// Show license tier, seat usage, activation health, expiry.
    License,
}

/// Components recognised by `computeza install`. Postgres has full
/// multi-OS coverage; the other 10 currently ship Linux-only drivers.
#[derive(clap::ValueEnum, Clone, Debug)]
enum InstallComponent {
    /// PostgreSQL.
    Postgres,
    /// Kanidm identity provider. Linux-only.
    Kanidm,
    /// Garage S3-compatible object storage. Linux-only.
    Garage,
    /// Lakekeeper Iceberg REST catalog. Linux-only.
    Lakekeeper,
    /// Apache XTable Iceberg<->Delta<->Hudi sync. Linux-only.
    Xtable,
    /// Databend columnar SQL engine. Linux-only.
    Databend,
    /// Qdrant vector store. Linux-only.
    Qdrant,
    /// Restate durable execution. Linux-only.
    Restate,
    /// GreptimeDB unified observability. Linux-only.
    Greptime,
    /// Grafana visualisation. Linux-only.
    Grafana,
    /// OpenFGA fine-grained authorization. Linux-only.
    Openfga,
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let l = Localizer::english();

    match cli.command {
        None => {
            println!("{}", l.t("welcome-banner"));
            println!("{}", l.t("welcome-help"));
        }
        Some(Command::Serve { addr, state_db }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let store = computeza_state::SqliteStore::open(&state_db)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "opening SqliteStore at {state_db}: {e}; \
                         pass --state-db <writable-path> to relocate (the default \
                         `./computeza-state.db` requires write permission in the CWD)"
                        )
                    })?;
                tracing::info!(state_db = %state_db, "metadata store ready");
                let state = computeza_ui_server::AppState::with_store(store);
                let store_for_tick = state
                    .store
                    .clone()
                    .expect("AppState::with_store always populates store");
                tokio::spawn(reconcile_tick(store_for_tick));
                computeza_ui_server::serve_with_state(addr, state).await
            })?;
        }
        Some(Command::Install { component }) => {
            install_component(component, &l)?;
        }
        Some(Command::Status { url }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(status(&url, &l))?;
        }
        Some(Command::License) => println!("{}", l.t("cmd-license-todo")),
    }
    Ok(())
}

/// Probe the operator console at `url` and print a localized status
/// summary. v0.0.x just hits `/healthz`; future versions surface drift
/// per resource (spec section 4.4 drift indicators).
async fn status(url: &str, l: &Localizer) -> anyhow::Result<()> {
    let healthz = format!("{}/healthz", url.trim_end_matches('/'));
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .get(&healthz)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            println!("{} ({url})", l.t("status-healthy"));
            Ok(())
        }
        Ok(r) => {
            println!("{}: HTTP {} ({url})", l.t("status-unhealthy"), r.status());
            std::process::exit(1);
        }
        Err(e) => {
            println!("{}: {e} ({url})", l.t("status-unreachable"));
            std::process::exit(1);
        }
    }
}

/// Dispatch `computeza install <component>` to the appropriate platform
/// install path. Postgres has full multi-OS coverage; the rest are
/// Linux-only today.
fn install_component(component: InstallComponent, l: &Localizer) -> anyhow::Result<()> {
    match component {
        InstallComponent::Postgres => install_postgres(l),
        InstallComponent::Kanidm => install_simple_linux("kanidm", l),
        InstallComponent::Garage => install_simple_linux("garage", l),
        InstallComponent::Lakekeeper => install_simple_linux("lakekeeper", l),
        InstallComponent::Xtable => install_simple_linux("xtable", l),
        InstallComponent::Databend => install_simple_linux("databend", l),
        InstallComponent::Qdrant => install_simple_linux("qdrant", l),
        InstallComponent::Restate => install_simple_linux("restate", l),
        InstallComponent::Greptime => install_simple_linux("greptime", l),
        InstallComponent::Grafana => install_simple_linux("grafana", l),
        InstallComponent::Openfga => install_simple_linux("openfga", l),
    }
}

/// Run a Linux single-binary-service install for the named component.
/// On non-Linux this is a clear-error stub until per-OS drivers land.
#[cfg(target_os = "linux")]
fn install_simple_linux(component: &str, _l: &Localizer) -> anyhow::Result<()> {
    use computeza_driver_native::linux;
    use computeza_driver_native::progress::ProgressHandle;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let progress = ProgressHandle::noop();
    let result = runtime.block_on(async move {
        match component {
            "kanidm" => linux::kanidm::install(linux::kanidm::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "garage" => linux::garage::install(linux::garage::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "lakekeeper" => {
                linux::lakekeeper::install(linux::lakekeeper::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "xtable" => linux::xtable::install(linux::xtable::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "databend" => {
                linux::databend::install(linux::databend::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "qdrant" => linux::qdrant::install(linux::qdrant::InstallOptions::default(), &progress)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .map(|r| (r.bin_dir.display().to_string(), r.port)),
            "restate" => {
                linux::restate::install(linux::restate::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "greptime" => {
                linux::greptime::install(linux::greptime::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "grafana" => {
                linux::grafana::install(linux::grafana::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            "openfga" => {
                linux::openfga::install(linux::openfga::InstallOptions::default(), &progress)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .map(|r| (r.bin_dir.display().to_string(), r.port))
            }
            other => Err(anyhow::anyhow!("unknown component: {other}")),
        }
    })?;
    println!(
        "{component} installed.\n  bin_dir: {}\n  port: {}",
        result.0, result.1
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_simple_linux(component: &str, _l: &Localizer) -> anyhow::Result<()> {
    anyhow::bail!(
        "`computeza install {component}` is currently implemented for Linux only. \
         The driver code is in `crates/computeza-driver-native/src/linux/{component}.rs`; \
         the Windows + macOS variants ship in follow-up commits. Postgres is the only \
         component with full multi-OS coverage so far."
    )
}

#[cfg(target_os = "linux")]
fn install_postgres(_l: &Localizer) -> anyhow::Result<()> {
    use computeza_driver_native::linux::postgres;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(postgres::install(postgres::InstallOptions::default()))?;
    println!(
        "PostgreSQL installed.\n  bin_dir: {}\n  data_dir: {}\n  unit: {}\n  port: {}",
        result.bin_dir.display(),
        result.data_dir.display(),
        result.unit_path.display(),
        result.port,
    );
    if let Some(link) = result.psql_symlink {
        println!("  psql:    {}", link.display());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_postgres(_l: &Localizer) -> anyhow::Result<()> {
    // PostgreSQL is Linux-only in v0.0.x to stay consistent with the
    // other 10 components. The macOS + Windows driver modules under
    // crates/computeza-driver-native/src/{macos,windows}/postgres.rs
    // are reference code from earlier iterations but no longer
    // reachable through the CLI or the wizard. Re-enable at v0.1+
    // when the macOS / Windows variants of the generic service
    // helper land and every component can be installed multi-OS
    // together.
    anyhow::bail!(
        "`computeza install postgres` is currently implemented for Linux only. \
         v0.0.x targets systemd-based Linux on x86_64 for the entire data plane. \
         macOS + Windows native install drivers ship in v0.1+. See README \
         'Platform support' for the supported distro list."
    )
}

/// Periodic in-process reconcile tick. Reads every `postgres-instance`
/// row from the metadata store and runs `observe()` against it,
/// persisting the result back via `with_state(store, name)`. The UI's
/// `/status` page then surfaces the live server version, last-observed
/// timestamp, and the green "Observing" badge.
///
/// v0.0.x only covers postgres-instance; the HTTP reconcilers
/// (kanidm/garage/lakekeeper/...) will join this loop once their
/// resource types are populated through the apply flow.
async fn reconcile_tick(store: std::sync::Arc<computeza_state::SqliteStore>) {
    use computeza_core::reconciler::Context;
    use computeza_core::{NoOpDriver, Reconciler};
    use computeza_reconciler_postgres::{PostgresReconciler, PostgresSpec};
    use computeza_state::Store;

    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    // Don't fire immediately on startup -- give the install path or the
    // operator time to write the first row.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let ctx = Context::default();
    loop {
        tick.tick().await;
        let rows = match store.list("postgres-instance", None).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reconcile_tick: failed to list postgres-instance resources; \
                     skipping this tick. Will retry on the next 30s interval."
                );
                continue;
            }
        };
        for sr in rows {
            let spec: PostgresSpec = match serde_json::from_value(sr.spec.clone()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        kind = %sr.key.kind,
                        name = %sr.key.name,
                        "reconcile_tick: spec did not deserialize as PostgresSpec; \
                         leaving status as Unknown. Inspect /resource/{}/{} and fix the spec.",
                        sr.key.kind, sr.key.name,
                    );
                    continue;
                }
            };
            let reconciler: PostgresReconciler<NoOpDriver> =
                PostgresReconciler::new(spec.endpoint.clone(), spec.superuser_password)
                    .with_state(store.clone(), &sr.key.name);
            // Observe is best-effort; the reconciler itself logs failures
            // and writes a `last_observe_failed=true` sentinel to the
            // store so the UI can render the orange "Failed" badge.
            let _ = reconciler.observe(&ctx).await;
        }
    }
}

/// Configure the global tracing subscriber. Reads `RUST_LOG` for filtering
/// (defaults to `info`) and writes structured logs to stderr.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
