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
    // Auto-install python3 + python3-venv if missing. Computeza
    // already runs as root for the install path (writing /etc/systemd
    // /var/lib/computeza/...) so the apt-get / dnf is no escalation.
    progress.set_message("Ensuring python3 + python3-venv".to_string());
    if let Err(e) = ensure_python_runtime().await {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "python3 / python3-venv install failed: {e}\n\n\
             Manual fix:\n  \
             Debian/Ubuntu: sudo apt install python3 python3-venv\n  \
             Fedora/RHEL:   sudo dnf install python3 python3-pip"
        ))));
    }
    let py = system_python().await.ok_or_else(|| {
        ServiceError::Io(std::io::Error::other(
            "python3 still not found after auto-install attempt. \
             Install manually: sudo apt install python3 python3-venv (Debian/Ubuntu) \
             or sudo dnf install python3 python3-pip (Fedora/RHEL).",
        ))
    })?;

    progress.set_message(format!("Creating venv {}", venv_dir.display()));
    let mut out = Command::new(&py)
        .args(["-m", "venv", venv_dir.to_str().unwrap_or("")])
        .output()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("venv: {e}"))))?;
    // First venv attempt failed -- typical cause is the version-
    // specific python3.X-venv package not pulled in by the bare
    // python3-venv meta-package. Auto-install python3<MAJ>-venv and
    // retry once before surfacing the manual-fix error.
    //
    // Note: Python's venv module writes its "apt install python3.X-venv"
    // hint to STDOUT, not stderr (empty stderr was the symptom that
    // bit us: the extractor only looked at stderr and missed the
    // pkg name). We now scan stderr + stdout together, plus also
    // derive the package name from `python3 --version` as a final
    // fallback when Python's hint format changes.
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let combined = format!("{stderr}\n{stdout}");
        let versioned_pkg = extract_versioned_venv_pkg_hint(&combined)
            .or_else(|| derive_versioned_venv_pkg_from_python(&py));
        if let Some(versioned_pkg) = versioned_pkg {
            tracing::warn!(
                pkg = %versioned_pkg,
                "sail install: venv failed; installing version-specific package and retrying"
            );
            if let Err(e) = apt_install(&versioned_pkg).await {
                tracing::warn!(error = %e, "sail install: apt-install {versioned_pkg} returned err; retrying venv anyway");
            }
            out = Command::new(&py)
                .args(["-m", "venv", venv_dir.to_str().unwrap_or("")])
                .output()
                .await
                .map_err(|e| ServiceError::Io(std::io::Error::other(format!("venv retry: {e}"))))?;
        }
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "python3 -m venv failed.\n  \
             Manual fix: sudo apt install python3-venv (or the version-specific \
             python3.X-venv shown in the error below).\n\n\
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
    // Also install PyIceberg + PyArrow so Studio's PyIceberg
    // execution path has a working catalog client in the same
    // venv. PyIceberg's a pure-Python wheel; PyArrow ships as a
    // platform wheel. Pinning ranges (not exact) so a security
    // patch upstream lands without a Computeza release.
    let pyiceberg_spec = "pyiceberg[pyarrow,s3fs]>=0.10";
    let out = Command::new(&venv_pip)
        .args([
            "install",
            "--no-cache-dir",
            &pysail_spec,
            &pyspark_spec,
            pyiceberg_spec,
        ])
        .output()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("pip install: {e}"))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "pip install pysail+pyspark-client+pyiceberg failed. Common causes:\n  \
             - No network access from the install host\n  \
             - PySail wheel not available for this CPU arch (only x86_64 + aarch64 ship today)\n  \
             - Python version mismatch (PySail requires Python 3.10+, PyIceberg 3.9+)\n\n\
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

/// Detect the host package manager + install python3 and the venv
/// module if either is missing. Best-effort: returns Err only when
/// we know there's no Python AND no working package manager to fix
/// it. Errors include both the package manager's output and a
/// human-readable hint.
async fn ensure_python_runtime() -> std::result::Result<(), String> {
    // Already installed? `python3 -m venv --help` exits 0 only when
    // both the interpreter AND the venv module are present, which
    // is exactly the combination we need.
    if Command::new("python3")
        .args(["-m", "venv", "--help"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Ok(());
    }
    // apt (Debian/Ubuntu) -- common case for our managed Linux
    // install path. dnf path is the fallback for RHEL/Fedora family.
    if Command::new("apt-get").arg("--version").output().await.map(|o| o.status.success()).unwrap_or(false) {
        // Run apt-get update first so we don't fail on stale indexes.
        // Lockfile-busy is rare for this short window; we don't
        // attempt to wait for /var/lib/dpkg/lock-frontend.
        let _ = Command::new("apt-get")
            .args(["update", "-qq"])
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .await;
        for pkg in ["python3", "python3-venv", "python3-pip"] {
            apt_install(pkg).await?;
        }
        // The bare `python3-venv` meta-package on newer Debian/Ubuntu
        // (e.g. Ubuntu noble/oracular shipping Python 3.14 as the
        // default) only pulls in the dependency stub; the actual
        // version-specific package `python3.X-venv` is required for
        // ensurepip to work. Probe the resolved python3 version and
        // install the matching package up front so the first
        // `python3 -m venv` call succeeds.
        if let Some(versioned) =
            derive_versioned_venv_pkg_from_python(std::path::Path::new("python3"))
        {
            if versioned != "python3-venv" {
                if let Err(e) = apt_install(&versioned).await {
                    // Not fatal: if apt doesn't have this specific
                    // package, the retry-on-failure path in the
                    // install fn will catch it.
                    tracing::warn!(
                        pkg = %versioned,
                        error = %e,
                        "ensure_python_runtime: version-specific venv pkg install non-fatal failure"
                    );
                }
            }
        }
        return Ok(());
    }
    if Command::new("dnf").arg("--version").output().await.map(|o| o.status.success()).unwrap_or(false) {
        let out = Command::new("dnf")
            .args(["install", "-y", "python3", "python3-pip"])
            .output()
            .await
            .map_err(|e| format!("dnf install: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "dnf install python3 python3-pip exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        return Ok(());
    }
    Err(
        "no package manager (apt-get/dnf) detected; install python3 + python3-venv manually \
         and re-run the Sail install"
            .into(),
    )
}

/// Best-effort `apt-get install -y <pkg>` with `DEBIAN_FRONTEND=
/// noninteractive` so it never prompts. Returns the apt error
/// verbatim on non-zero exit so the operator sees what apt actually
/// said.
async fn apt_install(pkg: &str) -> std::result::Result<(), String> {
    let out = Command::new("apt-get")
        .args(["install", "-y", "-qq", pkg])
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .await
        .map_err(|e| format!("apt-get install {pkg}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "apt-get install {pkg} exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Parse Debian's venv-failure stderr for the suggested versioned
/// package name. The system installs the `python3-venv` meta-package
/// but the resolver may still need `python3.X-venv` for the active
/// Python version. The error message includes the right package
/// name -- we extract it rather than guess from `python3 --version`.
///
/// Example stderr:
///   The virtual environment was not created successfully because
///   ensurepip is not available.  On Debian/Ubuntu systems, you need
///   to install the python3-venv package using the following command.
///       apt install python3.14-venv
fn extract_versioned_venv_pkg_hint(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        let t = line.trim();
        // Look for "apt install python3.X-venv" or similar.
        if let Some(rest) = t.strip_prefix("apt install ") {
            let pkg = rest.split_whitespace().next()?;
            if pkg.starts_with("python3") && pkg.ends_with("-venv") {
                return Some(pkg.to_string());
            }
        }
        if let Some(rest) = t.strip_prefix("apt-get install ") {
            let pkg = rest.split_whitespace().next()?;
            if pkg.starts_with("python3") && pkg.ends_with("-venv") {
                return Some(pkg.to_string());
            }
        }
    }
    None
}

/// Fallback: run `<py> --version` and derive the matching
/// `python3.X-venv` package name. Used when Python's stderr/stdout
/// don't include the helpful "apt install python3.X-venv" hint
/// (e.g. on Ubuntu where the hint goes to stdout, not stderr, and
/// the captured stdout was empty in the operator's report). Returns
/// None if `python3 --version` doesn't print a parseable version.
fn derive_versioned_venv_pkg_from_python(py: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(py)
        .arg("--version")
        .output()
        .ok()?;
    // python3 --version writes "Python 3.14.0" to stdout (and on
    // older versions, stderr -- check both).
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let version = combined.split_whitespace().find(|t| {
        t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('.')
    })?;
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    if major == "3" {
        Some(format!("python3.{minor}-venv"))
    } else {
        None
    }
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
