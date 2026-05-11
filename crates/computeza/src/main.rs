//! Computeza — single binary entry point.
//!
//! Per spec §2.1, this binary is three things:
//!
//! 1. **Installer** — first-run wizard that lays down the entire data plane
//!    natively on the host operating system (`computeza install`).
//! 2. **Operator console** — long-running server hosting the Leptos web UI,
//!    REST API, gRPC API, and the reconciliation loop (`computeza serve`).
//! 3. **Configurator** — declarative state engine with a YAML representation
//!    suitable for git, CI, and IaC workflows (`computeza apply`, etc.).
//!
//! Every command surface here is GUI-equivalent: anything you can do from
//! this CLI you can also do from the web console (and vice versa). The CLI
//! exists for power users, scripting, and CI; the web console is the
//! primary interface (spec §2.1, §4.2, and the user mandate that
//! everything — instances, clusters, users, permissions — be reachable
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
/// web console (spec §4.2).
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
        /// per the port allocation in spec §10.6.
        #[arg(long, default_value = "127.0.0.1:8400")]
        addr: SocketAddr,
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

/// Components recognised by `computeza install`.
#[derive(clap::ValueEnum, Clone, Debug)]
enum InstallComponent {
    /// PostgreSQL (Linux-only in v0.0.x).
    Postgres,
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
        Some(Command::Serve { addr }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(computeza_ui_server::serve(addr))?;
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
/// per resource (spec §4.4 drift indicators).
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
/// install path. v0.0.x is Linux-only; macOS / Windows print a localized
/// "not yet supported" message.
fn install_component(component: InstallComponent, l: &Localizer) -> anyhow::Result<()> {
    match component {
        InstallComponent::Postgres => install_postgres(l),
    }
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
fn install_postgres(l: &Localizer) -> anyhow::Result<()> {
    println!("{}", l.t("install-postgres-linux-only"));
    Ok(())
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
