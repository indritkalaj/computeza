//! LakeSail Sail -- Spark-API-compatible distributed compute engine
//! written in Rust. Linux install path.
//!
//! Unlike the other managed components, Sail does not ship pre-built
//! standalone binary tarballs; the official distribution is the
//! `pysail` wheel on PyPI which bundles the compiled Rust binary
//! inside a Python package. We install it into a dedicated venv
//! under `<root>/venv/` so:
//!
//!   1. The system Python install is untouched (operators on Debian
//!      hate pip-installing into system site-packages).
//!   2. Uninstall reduces to `rm -rf <root>/venv` -- no orphaned
//!      site-packages files.
//!   3. The pinned `pysail==<version>` is reproducible across
//!      re-installs.
//!
//! The systemd unit's `ExecStart` is `<root>/venv/bin/sail spark
//! server --port <port>`, which is the same entrypoint the upstream
//! Docker image uses. Spark Connect clients connect via
//! `sc://<host>:<port>` (gRPC).

use std::path::PathBuf;

use tokio::process::Command;

use crate::progress::{InstallPhase, ProgressHandle};

use super::service::{InstalledService, ServiceError, Uninstalled};
use super::{path as pathmod, systemctl};

pub const UNIT_NAME: &str = "computeza-sail.service";

/// Spark Connect gRPC port. 50051 is the upstream default; matches
/// the doc example `sail spark server --port 50051` and the
/// `sc://localhost:50051` connection string PySpark clients use.
pub const DEFAULT_PORT: u16 = 50051;

/// Pinned PySail version. PySail ships the compiled Rust binary
/// inside the wheel, so the version we pin here IS the Sail engine
/// version. The pyspark-client package is the Spark Connect Python
/// client used by Studio's Python execution path to submit code.
///
/// To bump: update DEFAULT_PYSAIL_VERSION, smoke-test
/// `<venv>/bin/sail --version`, and run the studio Python query
/// path against a freshly bootstrapped Lakekeeper warehouse.
const DEFAULT_PYSAIL_VERSION: &str = "0.6.2";
const DEFAULT_PYSPARK_CLIENT_VERSION: &str = "4.0.0";

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub unit_name: String,
    /// Explicit pysail version to install. None falls back to
    /// DEFAULT_PYSAIL_VERSION.
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/sail"),
            port: DEFAULT_PORT,
            unit_name: UNIT_NAME.into(),
            version: None,
        }
    }
}

/// Install Sail by:
///   1. Creating a Python venv under `<root>/venv/`
///   2. `pip install pysail==<version> pyspark-client==<x>`
///   3. Writing the systemd unit
///   4. systemctl enable --now
///   5. Waiting for port 50051 to accept gRPC connections
pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    let version = opts
        .version
        .clone()
        .unwrap_or_else(|| DEFAULT_PYSAIL_VERSION.to_string());
    let venv_dir = opts.root_dir.join("venv");
    // venv's pip + sail are referenced directly; the venv python
    // path is exposed via installed_venv_python() for the studio
    // executor and not needed inline here.
    let venv_pip = venv_dir.join("bin").join("pip");
    let venv_sail = venv_dir.join("bin").join("sail");

    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message(format!("Preparing {}", opts.root_dir.display()));
    tokio::fs::create_dir_all(&opts.root_dir).await?;

    // --- Step 1: python3 venv -------------------------------------
    // Bail loudly if python3 isn't installed -- the operator can fix
    // this with one apt-get and re-run install. Mentioning the
    // distro-specific package name in the error saves a Google.
    let py = system_python().await.ok_or_else(|| {
        ServiceError::Io(std::io::Error::other(
            "python3 not found on PATH. Install it first:\n  \
             Debian/Ubuntu: sudo apt install python3 python3-venv\n  \
             Fedora/RHEL:   sudo dnf install python3 python3-pip\n\
             Sail is distributed as a PyPI wheel and needs a Python interpreter.",
        ))
    })?;

    progress.set_message(format!("Creating venv {}", venv_dir.display()));
    let out = Command::new(&py)
        .args(["-m", "venv", venv_dir.to_str().unwrap_or("")])
        .output()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("venv: {e}"))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "python3 -m venv failed. Most likely cause: python3-venv package is missing.\n  \
             Debian/Ubuntu fix: sudo apt install python3-venv\n\n\
             stderr:\n{stderr}\n\nstdout:\n{stdout}"
        ))));
    }

    // --- Step 2: pip install pysail ------------------------------
    // Upgrade pip first so the wheel resolver doesn't 404 on newer
    // metadata formats (PySail uses PEP 621 + PEP 660).
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!("pip install pysail=={version}"));
    let out = Command::new(&venv_pip)
        .args(["install", "--upgrade", "pip"])
        .output()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("pip upgrade: {e}"))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "pip install --upgrade pip failed.\n\nstderr:\n{stderr}"
        ))));
    }

    let pysail_spec = format!("pysail=={version}");
    let pyspark_spec = format!("pyspark-client=={DEFAULT_PYSPARK_CLIENT_VERSION}");
    let out = Command::new(&venv_pip)
        .args(["install", "--no-cache-dir", &pysail_spec, &pyspark_spec])
        .output()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("pip install: {e}"))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "pip install pysail+pyspark-client failed. Common causes:\n  \
             - No network access from the install host\n  \
             - PySail wheel not available for this CPU arch (only x86_64 + aarch64 ship today)\n  \
             - Python version mismatch (PySail requires Python 3.10+)\n\n\
             stderr:\n{stderr}\n\nstdout:\n{stdout}"
        ))));
    }

    // Sanity-check the binary exists where we expect.
    if !tokio::fs::try_exists(&venv_sail).await.unwrap_or(false) {
        return Err(ServiceError::BinaryMissing {
            binary: "sail".into(),
            bin_dir: venv_dir.join("bin"),
        });
    }

    // --- Step 3: systemd unit ------------------------------------
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Writing systemd unit {}", opts.unit_name));
    let unit_body = format!(
        "[Unit]\n\
         Description=Computeza-managed LakeSail Sail (Spark Connect)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={sail} spark server --port {port}\n\
         WorkingDirectory={root}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        sail = venv_sail.display(),
        port = opts.port,
        root = opts.root_dir.display(),
    );
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    tokio::fs::write(&unit_path, unit_body).await?;
    systemctl::daemon_reload().await?;

    // --- Step 4: enable + start ----------------------------------
    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    let _ = systemctl::stop(&opts.unit_name).await;
    systemctl::enable_now(&opts.unit_name).await?;

    // --- Step 5: wait for port -----------------------------------
    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!("Waiting for Spark Connect on {}", opts.port));
    if let Err(e) = super::service::wait_for_port(
        "127.0.0.1",
        opts.port,
        std::time::Duration::from_secs(60),
    )
    .await
    {
        if matches!(e, ServiceError::NotReady { .. }) {
            let tail = systemctl::journal_tail(&opts.unit_name, 80).await;
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "sail did not bind 127.0.0.1:{} within 60s. Journal tail (most recent 80 lines from `journalctl -u {}`):\n\n{tail}",
                opts.port, opts.unit_name
            ))));
        }
        return Err(e);
    }

    // Register the venv's sail binary on PATH so operators can run
    // it as `computeza-sail` for ad-hoc work.
    let _ = pathmod::register("sail", &venv_sail).await;

    Ok(InstalledService {
        bin_dir: venv_dir.join("bin"),
        unit_path,
        port: opts.port,
        cli_symlink: None,
    })
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub unit_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/sail"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    // Reuse the shared uninstall sweep -- stops/disables/removes the
    // unit, wipes the entire root_dir (which includes our venv/),
    // unregisters the sail CLI symlink, sweeps /run/sail.
    super::service::uninstall_service("sail", &opts.root_dir, &opts.unit_name, Some("sail")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/sail");
    if !tokio::fs::try_exists(root.join("venv").join("bin").join("sail"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-sail".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: Some(root.clone()),
        bin_dir: Some(root.join("venv").join("bin")),
    }]
}

/// Path to the venv's python3, used by the studio Python query path
/// to spawn `sail` Spark Connect client code. Returns None if Sail
/// isn't installed (the studio router falls back to "Sail unavailable"
/// in that case).
pub fn installed_venv_python(root_dir: &std::path::Path) -> PathBuf {
    root_dir.join("venv").join("bin").join("python3")
}

/// Discover the system's `python3` binary. PySail ships wheels for
/// 3.10+; we let the upstream pip resolver decide if the discovered
/// interpreter is too old. We prefer `python3` over `python` because
/// most distros ship 2.x as `python` for legacy compat.
async fn system_python() -> Option<PathBuf> {
    for candidate in ["python3", "python3.12", "python3.11", "python3.10"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}
