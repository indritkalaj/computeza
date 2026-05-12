//! Shared install/uninstall helpers for single-binary services on Linux.
//!
//! Most managed components (kanidm, garage, qdrant, restate, openfga,
//! grafana, greptime, databend, lakekeeper) are single-binary services:
//! the install path is essentially "download binary, drop a systemd
//! unit, start it". This module factors that out so each component
//! lands as ~80 lines of configuration rather than ~300 lines of
//! reimplementation.
//!
//! Postgres is the special case (initdb, pg_hba, role bootstrap) and
//! keeps its own bespoke driver in `linux::postgres`.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tokio::{fs, net::TcpStream, time::sleep};
use tracing::{info, warn};

use super::{path as pathmod, systemctl};
use crate::{
    fetch::{self, Bundle, FetchError},
    progress::{InstallPhase, ProgressHandle},
};

/// Component-agnostic install configuration. The component-specific
/// driver constructs one of these from its CLI/UI inputs and hands it
/// to [`install_service`].
#[derive(Clone, Debug)]
pub struct ServiceInstall {
    /// Display name used in log lines (e.g. "kanidm", "garage").
    pub component: &'static str,
    /// Subdirectory under `/var/lib/computeza/` that owns the binary
    /// cache + data dir. Typically the component name.
    pub root_dir: PathBuf,
    /// Bundle to fetch + extract.
    pub bundle: Bundle,
    /// Name of the binary inside `bundle.bin_subpath` to launch.
    pub binary_name: &'static str,
    /// Additional CLI args for the service's `ExecStart` line. Wrapped
    /// into the systemd unit verbatim.
    pub args: Vec<String>,
    /// TCP port the service binds. Used for the readiness probe.
    pub port: u16,
    /// systemd unit name including the `.service` suffix.
    pub unit_name: String,
    /// Optional config-file body. Written under `root_dir/<config_filename>`.
    pub config: Option<ConfigFile>,
    /// Name of the CLI tool to symlink into `/usr/local/bin/computeza-<name>`.
    /// None means no PATH registration.
    pub cli_symlink: Option<CliSymlink>,
}

/// One config file laid down before service start.
#[derive(Clone, Debug)]
pub struct ConfigFile {
    /// Filename under `root_dir`. The full path becomes
    /// `<root_dir>/<filename>`.
    pub filename: String,
    /// Verbatim contents.
    pub contents: String,
}

/// PATH-shim registration for a CLI that ships in the bundle.
#[derive(Clone, Debug)]
pub struct CliSymlink {
    /// `computeza-<short_name>` is what gets dropped into `/usr/local/bin/`.
    pub short_name: &'static str,
    /// Name of the binary in `bundle.bin_subpath` to point the symlink at.
    pub binary_name: &'static str,
}

/// Result of a successful [`install_service`].
#[derive(Clone, Debug)]
pub struct InstalledService {
    /// Cache directory the binary tree was extracted to.
    pub bin_dir: PathBuf,
    /// Path the systemd unit was written to.
    pub unit_path: PathBuf,
    /// Port the service is now listening on.
    pub port: u16,
    /// Optional PATH symlink created.
    pub cli_symlink: Option<PathBuf>,
}

/// Generic install pipeline. Mirrors the postgres flow at a higher
/// level of abstraction:
///
/// 1. Resolve the binary directory (download bundle if needed).
/// 2. Write the config file under root_dir, if provided.
/// 3. Write a systemd unit at `/etc/systemd/system/<unit_name>`.
/// 4. daemon-reload + enable --now.
/// 5. Wait for the TCP port.
/// 6. Optionally register a `/usr/local/bin/computeza-<short_name>` symlink.
pub async fn install_service(
    opts: ServiceInstall,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message(format!(
        "Fetching {} {} binaries",
        opts.component, opts.bundle.version
    ));
    let cache_root = opts.root_dir.join("binaries");
    let bin_dir = fetch::fetch_and_extract(&cache_root, &opts.bundle, progress).await?;
    if !fs::try_exists(bin_dir.join(opts.binary_name))
        .await
        .unwrap_or(false)
    {
        return Err(ServiceError::BinaryMissing {
            binary: opts.binary_name.into(),
            bin_dir: bin_dir.clone(),
        });
    }

    if let Some(cfg) = &opts.config {
        progress.set_message(format!(
            "Writing config {}/{}",
            opts.root_dir.display(),
            cfg.filename
        ));
        fs::create_dir_all(&opts.root_dir).await?;
        fs::write(opts.root_dir.join(&cfg.filename), &cfg.contents).await?;
    }

    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let bin_path = bin_dir.join(opts.binary_name);
    let args_str = opts.args.join(" ");
    let unit_body = systemd_unit(opts.component, &bin_path, &args_str, &opts.root_dir);
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    fs::write(&unit_path, &unit_body).await?;
    info!(unit = %unit_path.display(), "wrote systemd unit");
    systemctl::daemon_reload().await?;

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {} to accept connections",
        opts.port
    ));
    wait_for_port("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

    let cli_symlink = if let Some(cli) = &opts.cli_symlink {
        progress.set_phase(InstallPhase::RegisteringPath);
        progress.set_message(format!("Registering CLI {}", cli.short_name));
        match pathmod::register(cli.short_name, &bin_dir.join(cli.binary_name)).await {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(error = %e, "PATH registration failed; install otherwise complete");
                None
            }
        }
    } else {
        None
    };

    Ok(InstalledService {
        bin_dir,
        unit_path,
        port: opts.port,
        cli_symlink,
    })
}

/// Component-agnostic uninstall pipeline mirroring `install_service`.
///
/// Best-effort and idempotent: every step swallows "already gone" errors.
pub async fn uninstall_service(
    component: &str,
    root_dir: &Path,
    unit_name: &str,
    cli_short_name: Option<&str>,
) -> Result<Uninstalled, ServiceError> {
    let mut out = Uninstalled::default();

    if let Err(e) = systemctl::stop(unit_name).await {
        out.warn(format!("systemctl stop {unit_name}: {e}"));
    } else {
        out.ok(format!("stopped {unit_name}"));
    }
    if let Err(e) = systemctl::run(&["disable", unit_name]).await {
        out.warn(format!("systemctl disable {unit_name}: {e}"));
    } else {
        out.ok(format!("disabled {unit_name}"));
    }
    let unit_path = PathBuf::from("/etc/systemd/system").join(unit_name);
    if fs::try_exists(&unit_path).await.unwrap_or(false) {
        match fs::remove_file(&unit_path).await {
            Ok(()) => out.ok(format!("removed unit file {}", unit_path.display())),
            Err(e) => out.warn(format!("removing unit file {}: {e}", unit_path.display())),
        }
        if let Err(e) = systemctl::daemon_reload().await {
            out.warn(format!("daemon-reload: {e}"));
        } else {
            out.ok("daemon-reload");
        }
    }

    // Data dir lives under root_dir/data or root_dir itself depending
    // on the component. We delete root_dir/data if present, else the
    // top-level root_dir contents.
    let data_dir = root_dir.join("data");
    if fs::try_exists(&data_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(&data_dir).await {
            Ok(()) => out.ok(format!("removed data dir {}", data_dir.display())),
            Err(e) => out.warn(format!("removing data dir {}: {e}", data_dir.display())),
        }
    }

    if let Some(name) = cli_short_name {
        if let Err(e) = pathmod::unregister(name).await {
            out.warn(format!("removing {name} symlink: {e}"));
        } else {
            out.ok(format!("removed /usr/local/bin/computeza-{name}"));
        }
    }
    let _ = component;
    Ok(out)
}

#[derive(Clone, Debug, Default)]
pub struct Uninstalled {
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

impl Uninstalled {
    pub fn ok(&mut self, msg: impl Into<String>) {
        self.steps.push(msg.into());
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// Errors raised by [`install_service`] / [`uninstall_service`].
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Systemctl(#[from] systemctl::SystemctlError),
    #[error("expected binary {binary:?} not found under {}", bin_dir.display())]
    BinaryMissing { binary: String, bin_dir: PathBuf },
    #[error("service did not become ready on port {port} within {timeout_secs}s")]
    NotReady { port: u16, timeout_secs: u64 },
}

fn systemd_unit(component: &str, bin_path: &Path, args: &str, root_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Computeza-managed {component}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} {args}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ReadWritePaths={root}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        component = component,
        bin = bin_path.display(),
        args = args,
        root = root_dir.display(),
    )
}

async fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<(), ServiceError> {
    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::NotReady {
                port,
                timeout_secs: timeout.as_secs(),
            });
        }
        sleep(Duration::from_millis(500)).await;
    }
}

// Suppress the `Pin` warning we don't actually need here.
#[allow(unused_imports)]
use tokio::io::AsyncWriteExt as _;
