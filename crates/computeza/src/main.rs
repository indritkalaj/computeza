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
    /// First-run installer wizard.
    Install,
    /// Start the operator console (web UI + reconciler loop).
    Serve {
        /// Address to bind the HTTP server to. Default `127.0.0.1:8400`
        /// per the port allocation in spec §10.6.
        #[arg(long, default_value = "127.0.0.1:8400")]
        addr: SocketAddr,
    },
    /// Show cluster status and reconciliation drift.
    Status,
    /// Show license tier, seat usage, activation health, expiry.
    License,
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
        Some(Command::Install) => println!("{}", l.t("cmd-install-todo")),
        Some(Command::Status) => println!("{}", l.t("cmd-status-todo")),
        Some(Command::License) => println!("{}", l.t("cmd-license-todo")),
    }
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
