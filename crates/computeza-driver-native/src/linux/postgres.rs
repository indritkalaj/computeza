//! Native installation of PostgreSQL on Linux via systemd.
//!
//! This is the first end-to-end demonstration of the autonomous-installer
//! mandate (spec section 2.1). The installer:
//!
//! 1. Resolves the `postgres` and `initdb` binaries via the following
//!    priority order:
//!    - Explicit `opts.bin_dir` override (operator pointed at a custom
//!      install).
//!    - Distro package binaries -- typically `/usr/lib/postgresql/<v>/bin`
//!      (Debian / Ubuntu) or `/usr/pgsql-<v>/bin` (RHEL / Fedora /
//!      OpenSUSE), majors 13 to 18.
//!    - Distro-aware auto-install via the host's package manager
//!      (`apt`, `dnf`, `zypper`, `pacman`). When no PostgreSQL is
//!      found and we're running as root, the driver runs the
//!      appropriate `apt install postgresql` (or equivalent) and
//!      re-scans for the freshly-installed binaries. EnterpriseDB
//!      used to publish portable Linux tarballs; they stopped
//!      around 2023 and the only remaining cross-distro path is to
//!      lean on the package manager. v0.1+ will additionally ship
//!      Computeza-built portable Postgres binaries via GitHub
//!      Releases so even the package-manager step goes away.
//! 2. Ensures the `postgres` system user exists (created via `useradd
//!    --system --no-create-home` when missing). The apt / yum packages
//!    create it as a side effect; when we resolved binaries via EDB
//!    fetch we have to do it ourselves.
//! 3. Creates a `postgres`-owned data directory at
//!    `/var/lib/computeza/postgres/data` and runs `initdb` against it
//!    (idempotent: skipped if the directory is already initialised).
//! 4. Writes a systemd unit at
//!    `/etc/systemd/system/computeza-postgres.service` and reloads the
//!    systemd manager.
//! 5. `systemctl enable --now computeza-postgres.service`.
//! 6. Waits until Postgres accepts TCP connections on the configured port.
//! 7. Registers a `computeza-psql` symlink under `/usr/local/bin/` per
//!    AGENTS.md rule section 4 (cross-platform PATH registration).
//!
//! Privileged operations (writing under `/var/lib`, `/etc/systemd/system`,
//! `/usr/local/bin`, running `initdb` as the postgres user, `systemctl`,
//! `useradd`) mean this code expects to be invoked while the binary is
//! running as root. The wrapping `computeza install` subcommand re-execs
//! itself with `sudo` when not already root.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, net::TcpStream, process::Command, time::sleep};
use tracing::{debug, info, warn};

use super::{path, systemctl};
use crate::progress::{InstallPhase, ProgressHandle};

/// Configuration for [`install`].
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Directory that will hold the PostgreSQL data files. Default
    /// `/var/lib/computeza/postgres`. The actual data lives in a `data/`
    /// subdirectory; everything else (`log/`, `run/`, `binaries/<v>/`)
    /// is alongside.
    pub root_dir: PathBuf,
    /// Where to find the `postgres` / `initdb` binaries. None means
    /// auto-detect by scanning common locations and falling through to
    /// fetching the Computeza-bundled EDB tarball.
    pub bin_dir: Option<PathBuf>,
    /// TCP port to listen on. Default 5432.
    pub port: u16,
    /// System user that owns the data directory and runs the daemon.
    /// Default `postgres`.
    pub system_user: String,
    /// Name of the systemd unit. Default `computeza-postgres.service`.
    pub unit_name: String,
    /// Major version to request from the host package manager when
    /// no PostgreSQL is detected and the driver falls through to
    /// `apt install postgresql-<major>` (or equivalent). `None`
    /// defaults to whichever major the distro ships as the
    /// unversioned `postgresql` meta-package -- the safest choice
    /// because it tracks the distro's tested release.
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/postgres"),
            bin_dir: None,
            port: 5432,
            system_user: "postgres".into(),
            unit_name: "computeza-postgres.service".into(),
            version: None,
        }
    }
}

/// PostgreSQL major versions the UI surfaces for the install
/// form. `None` (= "distro default") is the safest choice: the host
/// package manager picks whichever major the distro tests against,
/// and that's the major Computeza will probe first. The explicit
/// majors below cover the supported window for operators who need
/// to pin to a specific line.
#[must_use]
pub fn available_majors() -> &'static [&'static str] {
    &["distro-default", "18", "17", "16", "15"]
}

/// Host package managers the auto-install fallback knows how to
/// drive. Detected at runtime via `which`; the first one found wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageManager {
    /// Debian, Ubuntu, derivatives.
    Apt,
    /// Fedora, RHEL 8+, Rocky, AlmaLinux, derivatives.
    Dnf,
    /// SUSE, openSUSE.
    Zypper,
    /// Arch Linux, derivatives.
    Pacman,
}

impl PackageManager {
    fn binary(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
            Self::Pacman => "pacman",
        }
    }

    /// The argv used to install the PostgreSQL server + initdb.
    ///
    /// The package names differ slightly across families; the apt
    /// path supports a per-major suffix (`postgresql-17`) because
    /// Ubuntu/Debian ship multiple majors side-by-side under
    /// `/usr/lib/postgresql/<v>/`. The dnf/zypper/pacman paths use
    /// the meta-package and let the distro pick the major.
    fn install_argv(self, major: Option<&str>) -> Vec<String> {
        match self {
            Self::Apt => {
                let pkg = match major {
                    Some(m) if m != "distro-default" => format!("postgresql-{m}"),
                    _ => "postgresql".to_string(),
                };
                vec![
                    "apt-get".into(),
                    "install".into(),
                    "-y".into(),
                    pkg,
                    "postgresql-contrib".into(),
                ]
            }
            Self::Dnf => vec![
                "dnf".into(),
                "install".into(),
                "-y".into(),
                "postgresql-server".into(),
                "postgresql-contrib".into(),
            ],
            Self::Zypper => vec![
                "zypper".into(),
                "install".into(),
                "-y".into(),
                "postgresql-server".into(),
                "postgresql-contrib".into(),
            ],
            Self::Pacman => vec![
                "pacman".into(),
                "-S".into(),
                "--noconfirm".into(),
                "postgresql".into(),
            ],
        }
    }
}

/// Probe `$PATH` for a supported package manager.
async fn detect_package_manager() -> Option<PackageManager> {
    for pm in [
        PackageManager::Apt,
        PackageManager::Dnf,
        PackageManager::Zypper,
        PackageManager::Pacman,
    ] {
        let out = Command::new("which").arg(pm.binary()).output().await.ok();
        if out.is_some_and(|o| o.status.success()) {
            return Some(pm);
        }
    }
    None
}

/// Information returned by a successful [`install`].
#[derive(Clone, Debug)]
pub struct Installed {
    /// Resolved binary directory (`postgres` and `initdb` are here).
    pub bin_dir: PathBuf,
    /// Resolved data directory (the one passed to `postgres -D`).
    pub data_dir: PathBuf,
    /// Path to the systemd unit file we wrote.
    pub unit_path: PathBuf,
    /// Port the daemon is now listening on.
    pub port: u16,
    /// Symlink we created in `/usr/local/bin/` for `psql`.
    pub psql_symlink: Option<PathBuf>,
}

/// Errors from the install pipeline.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Filesystem / process I/O.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// We could not find `postgres` / `initdb` anywhere we looked.
    #[error("postgres binaries not found; tried: {0:?}")]
    BinaryNotFound(Vec<PathBuf>),
    /// `initdb` failed.
    #[error("initdb failed (exit {code:?}): {stderr}")]
    InitdbFailed {
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// systemctl call failed.
    #[error(transparent)]
    Systemctl(#[from] systemctl::SystemctlError),
    /// PATH registration failed.
    #[error(transparent)]
    Path(#[from] path::PathError),
    /// The distro-aware auto-install fallback failed: either no
    /// supported package manager was on $PATH, the install command
    /// exited non-zero, or the freshly-installed package didn't
    /// leave binaries where we expected to find them. Carries the
    /// full operator-facing message so the install UI surfaces it
    /// verbatim.
    #[error("auto-install via host package manager failed: {0}")]
    AutoInstall(String),
    /// Server never started accepting connections.
    #[error("postgres did not become ready on port {port} within {timeout_secs}s")]
    NotReady {
        /// Port we were waiting on.
        port: u16,
        /// How long we waited.
        timeout_secs: u64,
    },
}

/// Common locations a system Postgres install might leave its binaries.
/// Keep the major-version list in sync with `detect_installed`'s scan
/// loop -- both must agree on which majors Computeza recognises.
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "/usr/lib/postgresql/18/bin",
    "/usr/lib/postgresql/17/bin",
    "/usr/lib/postgresql/16/bin",
    "/usr/lib/postgresql/15/bin",
    "/usr/lib/postgresql/14/bin",
    "/usr/lib/postgresql/13/bin",
    "/usr/pgsql-18/bin",
    "/usr/pgsql-17/bin",
    "/usr/pgsql-16/bin",
    "/usr/pgsql-15/bin",
    "/usr/pgsql-14/bin",
    "/usr/pgsql-13/bin",
    "/opt/postgresql/bin",
    "/usr/bin",
    "/usr/local/bin",
];

/// Discover already-installed PostgreSQL instances on the host.
/// Conservative: reports installs we can verify on disk.
///
/// Sources inspected:
/// - Distro package binaries: `/usr/lib/postgresql/<v>/bin/postgres`.
/// - System data dir + version file: `/var/lib/postgresql/<v>/main/PG_VERSION`.
/// - Computeza-managed data dirs under `/var/lib/computeza/postgres*/data`.
pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let mut out = Vec::new();

    // Distro packages -- iterate the same majors as CANDIDATE_BIN_DIRS
    // and report each one whose binary actually exists.
    for major in [18u8, 17, 16, 15, 14, 13] {
        let bin = PathBuf::from(format!("/usr/lib/postgresql/{major}/bin"));
        if !fs::try_exists(bin.join("postgres")).await.unwrap_or(false) {
            continue;
        }
        let data_dir = PathBuf::from(format!("/var/lib/postgresql/{major}/main"));
        let (version, port) = inspect_data_dir_linux(&data_dir).await;
        out.push(crate::detect::DetectedInstall {
            identifier: format!("PostgreSQL {major} (apt/yum)"),
            owner: "distro package".into(),
            version: version.or(Some(major.to_string())),
            port,
            data_dir: Some(data_dir),
            bin_dir: Some(bin),
        });
    }

    // Computeza-managed data dirs.
    if let Ok(mut entries) = fs::read_dir("/var/lib/computeza").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !name.starts_with("postgres") {
                continue;
            }
            let data_dir = entry.path().join("data");
            if !fs::try_exists(&data_dir).await.unwrap_or(false) {
                continue;
            }
            let (version, port) = inspect_data_dir_linux(&data_dir).await;
            out.push(crate::detect::DetectedInstall {
                identifier: entry
                    .path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| name.clone()),
                owner: "computeza".into(),
                version,
                port,
                data_dir: Some(data_dir),
                bin_dir: None,
            });
        }
    }

    out
}

async fn inspect_data_dir_linux(data_dir: &Path) -> (Option<String>, Option<u16>) {
    let version = match fs::read_to_string(data_dir.join("PG_VERSION")).await {
        Ok(s) => Some(s.trim().to_string()),
        Err(_) => None,
    };
    let port = match fs::read_to_string(data_dir.join("postgresql.conf")).await {
        Ok(conf) => parse_port_from_postgresql_conf(&conf),
        Err(_) => None,
    };
    (version, port)
}

/// Shared parser. Mirrors the Windows implementation.
fn parse_port_from_postgresql_conf(conf: &str) -> Option<u16> {
    for line in conf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("port") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.split_whitespace().next()?;
        let value = value.trim_matches(|c| c == '\'' || c == '"');
        if let Ok(p) = value.parse::<u16>() {
            return Some(p);
        }
    }
    None
}

/// Configuration for [`uninstall`]. Mirrors the Windows variant so
/// per-OS install paths converge on a single uninstall contract from
/// the UI's point of view.
#[derive(Clone, Debug)]
pub struct UninstallOptions {
    /// Root the install used. Same default as [`InstallOptions::root_dir`].
    pub root_dir: PathBuf,
    /// Systemd unit name. Default `computeza-postgres.service`.
    pub unit_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/postgres"),
            unit_name: "computeza-postgres.service".into(),
        }
    }
}

/// Summary returned by [`uninstall`]. Same shape as the Windows
/// variant so the UI handler is OS-agnostic.
#[derive(Clone, Debug, Default)]
pub struct Uninstalled {
    /// Steps that completed successfully.
    pub steps: Vec<String>,
    /// Steps that failed (non-fatal -- the uninstall keeps going).
    pub warnings: Vec<String>,
}

impl Uninstalled {
    fn ok(&mut self, msg: impl Into<String>) {
        self.steps.push(msg.into());
    }
    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// Tear down a Linux PostgreSQL install written by [`install`].
///
/// Best-effort and idempotent: each step swallows "already gone"
/// errors so the caller gets a coherent summary regardless of what
/// state the prior install left behind.
///
/// What gets removed:
/// - systemd unit (stop, disable, file removal, daemon-reload).
/// - Data directory at `root_dir/data`.
/// - `/usr/local/bin/computeza-psql` symlink.
///
/// What is preserved:
/// - The host's PostgreSQL package (we never installed it -- v0.0.x
///   uses the distro-shipped binaries).
/// - The metadata-store row -- the caller deletes that separately.
pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, InstallError> {
    let mut out = Uninstalled::default();

    // 1. Service teardown via systemctl.
    if let Err(e) = systemctl::stop(&opts.unit_name).await {
        out.warn(format!("systemctl stop {}: {e}", opts.unit_name));
    } else {
        out.ok(format!("stopped {}", opts.unit_name));
    }
    if let Err(e) = systemctl::run(&["disable", &opts.unit_name]).await {
        out.warn(format!("systemctl disable {}: {e}", opts.unit_name));
    } else {
        out.ok(format!("disabled {}", opts.unit_name));
    }

    // 2. Remove the unit file and reload the manager so systemctl
    //    forgets about it.
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    if fs::try_exists(&unit_path).await.unwrap_or(false) {
        match fs::remove_file(&unit_path).await {
            Ok(()) => out.ok(format!("removed unit file {}", unit_path.display())),
            Err(e) => out.warn(format!("removing unit file {}: {e}", unit_path.display())),
        }
        if let Err(e) = systemctl::daemon_reload().await {
            out.warn(format!("systemctl daemon-reload: {e}"));
        } else {
            out.ok("systemctl daemon-reload");
        }
    }

    // 3. Data directory.
    let data_dir = opts.root_dir.join("data");
    if fs::try_exists(&data_dir).await.unwrap_or(false) {
        match fs::remove_dir_all(&data_dir).await {
            Ok(()) => out.ok(format!("removed data dir {}", data_dir.display())),
            Err(e) => out.warn(format!("removing data dir {}: {e}", data_dir.display())),
        }
    } else {
        out.ok(format!("data dir absent ({})", data_dir.display()));
    }

    // 4. PATH symlink.
    if let Err(e) = path::unregister("psql").await {
        out.warn(format!("removing psql symlink: {e}"));
    } else {
        out.ok("removed /usr/local/bin/computeza-psql");
    }

    Ok(out)
}

/// Auto-detect a binary directory that contains both `postgres` and `initdb`.
async fn detect_bin_dir() -> Result<PathBuf, InstallError> {
    let mut tried = Vec::new();
    for c in CANDIDATE_BIN_DIRS {
        let dir = PathBuf::from(c);
        tried.push(dir.clone());
        if fs::try_exists(dir.join("postgres")).await.unwrap_or(false)
            && fs::try_exists(dir.join("initdb")).await.unwrap_or(false)
        {
            return Ok(dir);
        }
    }
    Err(InstallError::BinaryNotFound(tried))
}

/// Install Postgres natively. Convenience entry point that wires a
/// no-op progress handle; the UI server uses
/// [`install_with_progress`] for streamed updates.
pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    install_with_progress(opts, &ProgressHandle::noop()).await
}

/// Install Postgres natively with streamed install-phase + byte
/// progress updates.
///
/// Compared to [`install`], this surfaces per-step status (binary
/// resolution, EDB fetch + extract, initdb, systemd registration,
/// startup wait) through the supplied [`ProgressHandle`] so the
/// operator console can render a live progress bar instead of a long
/// quiet pause during the fetch.
pub async fn install_with_progress(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<Installed, InstallError> {
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Resolving the postgres binary directory");
    let bin_dir = resolve_bin_dir(&opts, progress).await?;
    info!(bin_dir = %bin_dir.display(), "resolved postgres binaries");

    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message(format!(
        "Ensuring the `{}` system user exists for the data directory",
        opts.system_user
    ));
    ensure_system_user(&opts.system_user).await?;

    let data_dir = opts.root_dir.join("data");

    progress.set_phase(InstallPhase::Extracting);
    progress.set_message(format!(
        "Preparing the data directory at {}",
        data_dir.display()
    ));
    create_data_dir(&data_dir, &opts.system_user).await?;

    progress.set_message(format!("Running initdb against {}", data_dir.display()));
    run_initdb_if_needed(&bin_dir, &data_dir, &opts.system_user).await?;

    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let unit_path = write_systemd_unit(
        &opts.unit_name,
        &bin_dir,
        &data_dir,
        &opts.system_user,
        opts.port,
    )
    .await?;

    systemctl::daemon_reload().await?;

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {} on port {}", opts.unit_name, opts.port));
    systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for postgres to accept TCP on 127.0.0.1:{}",
        opts.port
    ));
    wait_for_ready("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

    let psql = bin_dir.join("psql");
    let psql_symlink = match path::register("psql", &psql).await {
        Ok(p) => Some(p),
        Err(e) => {
            // Not fatal: install succeeded, just couldn't symlink.
            warn!(error = %e, "registering /usr/local/bin/computeza-psql failed");
            None
        }
    };

    info!(port = opts.port, "postgres install complete");
    Ok(Installed {
        bin_dir,
        data_dir,
        unit_path,
        port: opts.port,
        psql_symlink,
    })
}

/// Resolve the binary directory in priority order: explicit override,
/// distro package, then distro-aware auto-install via the host
/// package manager.
async fn resolve_bin_dir(
    opts: &InstallOptions,
    progress: &ProgressHandle,
) -> Result<PathBuf, InstallError> {
    if let Some(d) = &opts.bin_dir {
        return Ok(d.clone());
    }
    match detect_bin_dir().await {
        Ok(d) => Ok(d),
        Err(InstallError::BinaryNotFound(tried)) => {
            // No distro postgres present yet. Drive the host
            // package manager to install one (apt / dnf / zypper /
            // pacman) and re-probe.
            auto_install_via_package_manager(opts.version.as_deref(), progress).await?;
            detect_bin_dir().await.map_err(|e| match e {
                InstallError::BinaryNotFound(_) => InstallError::AutoInstall(format!(
                    "package manager reported success but postgres binaries still not found in any of {tried:?}. The package may have installed to a non-standard path; pass `bin_dir` on the install form to point at the directory containing `postgres` and `initdb`."
                )),
                other => other,
            })
        }
        Err(e) => Err(e),
    }
}

/// Drive the host package manager to install PostgreSQL.
///
/// Sequence:
/// 1. Detect which package manager is on `$PATH`.
/// 2. Run its install command for the operator-selected major (or
///    the meta-package when no major was pinned).
/// 3. Surface stderr verbatim on failure so the operator sees why
///    it failed (commonly: not running as root, or no internet
///    egress to the distro mirror).
async fn auto_install_via_package_manager(
    major: Option<&str>,
    progress: &ProgressHandle,
) -> Result<(), InstallError> {
    // Fail fast on non-root. Every supported package manager
    // (apt-get / dnf / zypper / pacman) needs root to touch
    // /var/lib/apt or /var/cache/dnf. Calling them anyway just
    // surfaces a cryptic permission-denied error from the package
    // manager; we'd rather tell the operator the actionable thing
    // up front.
    //
    // We shell out to `id -u` rather than libc::geteuid() because
    // the workspace forbids `unsafe` (see Cargo.toml workspace
    // lints). `id` is in coreutils on every supported distro;
    // failure to find it implies a host so broken we shouldn't
    // try to autorun apt-get on it anyway.
    let euid_out = Command::new("id").arg("-u").output().await?;
    let euid_str = String::from_utf8_lossy(&euid_out.stdout).trim().to_string();
    let euid: u32 = euid_str.parse().unwrap_or(u32::MAX);
    if euid != 0 {
        return Err(InstallError::AutoInstall(format!(
            "no host postgres found, and the auto-install fallback needs root to drive the host package manager. The process is running as uid {euid} (non-root). Stop the operator console (Ctrl-C) and restart it under sudo, preserving your environment:\n\n    sudo -E ./target/release/computeza serve --addr 127.0.0.1:8400 --state-db /var/lib/computeza/computeza-state.db\n\nThe `-E` flag preserves COMPUTEZA_SECRETS_PASSPHRASE across the sudo boundary so the encrypted secrets store stays attached. Alternatively, install postgres manually (`sudo apt install postgresql postgresql-contrib`) and re-submit the install form -- the driver will detect the distro binaries and skip this fallback."
        )));
    }

    let pm = detect_package_manager().await.ok_or_else(|| {
        InstallError::AutoInstall(
            "no supported package manager on $PATH (looked for apt-get, dnf, zypper, pacman). Either install postgres manually (`apt install postgresql` / `dnf install postgresql-server` / etc.) and re-submit the install form, or pass `bin_dir` on the form to point at an existing postgres install.".into(),
        )
    })?;

    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!(
        "No distro postgres found. Auto-installing via {} (one-time; pulls ~50-80 MiB from your distro's mirror).",
        pm.binary()
    ));

    // apt-get needs `update` before `install` on a fresh container
    // where the package cache is stale. Other managers refresh on
    // every invocation.
    if pm == PackageManager::Apt {
        let upd = Command::new("apt-get")
            .arg("update")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !upd.status.success() {
            return Err(InstallError::AutoInstall(format!(
                "`apt-get update` failed (exit {:?}): {}. Rerun the install as root or fix the apt sources (commonly: no network egress to archive.ubuntu.com).",
                upd.status.code(),
                String::from_utf8_lossy(&upd.stderr).trim()
            )));
        }
    }

    let argv = pm.install_argv(major);
    let argv_display = argv.join(" ");
    info!(cmd = %argv_display, "running package-manager install for postgres");

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // dnf needs DNF_NOPLUGINS=0 to skip slow plugins on minimal images; safe to ignore otherwise.
    let out = cmd.output().await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(InstallError::AutoInstall(format!(
            "`{argv_display}` failed (exit {:?}). Common causes: not running as root (re-run with `sudo -E`), no internet egress to your distro's package mirror, or the requested major isn't packaged for this distro. Full stderr below:\n{stderr}",
            out.status.code(),
        )));
    }

    info!(cmd = %argv_display, "postgres package install complete");
    Ok(())
}

/// Ensure the `postgres` system user exists. apt/yum packages create
/// it as a side effect; when we resolved binaries via the EDB fetch
/// path the user might not exist and `initdb` + the systemd unit
/// would both fail otherwise.
///
/// `useradd` exits with status 9 when the account already exists,
/// which we treat as success. Anything else surfaces as an
/// [`InstallError::Io`] so the operator sees why creation failed
/// (commonly: not running as root).
async fn ensure_system_user(user: &str) -> Result<(), InstallError> {
    // Cheap probe first to avoid shelling out unnecessarily.
    let probe = Command::new("id").arg(user).output().await?;
    if probe.status.success() {
        return Ok(());
    }
    let status = Command::new("useradd")
        .arg("--system")
        .arg("--no-create-home")
        .arg("--shell")
        .arg("/usr/sbin/nologin")
        .arg("--user-group")
        .arg(user)
        .status()
        .await?;
    // useradd returns 9 when the user already exists -- harmless.
    if status.success() || status.code() == Some(9) {
        Ok(())
    } else {
        Err(InstallError::Io(io::Error::other(format!(
            "useradd --system {user} failed (exit {:?}); rerun the install as root or pre-create the user with `sudo useradd --system --no-create-home --shell /usr/sbin/nologin {user}`",
            status.code(),
        ))))
    }
}

async fn create_data_dir(data_dir: &Path, user: &str) -> Result<(), InstallError> {
    if let Some(parent) = data_dir.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::create_dir_all(data_dir).await?;
    // chown the parent + data dir to the system user. `chown -R` is the
    // simplest correct invocation; doing it via libc would require
    // resolving the uid/gid ourselves.
    let parent = data_dir.parent().unwrap_or(data_dir);
    let status = Command::new("chown")
        .arg("-R")
        .arg(format!("{user}:{user}"))
        .arg(parent)
        .status()
        .await?;
    if !status.success() {
        return Err(InstallError::Io(io::Error::other(format!(
            "chown -R {user}:{user} {parent:?} failed"
        ))));
    }
    // Postgres refuses to run on a data dir with permissive permissions.
    let _ = Command::new("chmod")
        .arg("0700")
        .arg(data_dir)
        .status()
        .await;
    Ok(())
}

async fn run_initdb_if_needed(
    bin_dir: &Path,
    data_dir: &Path,
    user: &str,
) -> Result<(), InstallError> {
    let marker = data_dir.join("PG_VERSION");
    if fs::try_exists(&marker).await? {
        debug!(data_dir = %data_dir.display(), "data dir already initialised; skipping initdb");
        return Ok(());
    }
    info!(data_dir = %data_dir.display(), "running initdb");
    let mut cmd = Command::new("runuser");
    cmd.arg("-u")
        .arg(user)
        .arg("--")
        .arg(bin_dir.join("initdb"))
        .arg("-D")
        .arg(data_dir)
        .arg("--auth-host=scram-sha-256")
        .arg("--auth-local=peer")
        .arg("--encoding=UTF8")
        .arg("--locale=C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(InstallError::InitdbFailed {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

async fn write_systemd_unit(
    unit_name: &str,
    bin_dir: &Path,
    data_dir: &Path,
    user: &str,
    port: u16,
) -> Result<PathBuf, InstallError> {
    // RuntimeDirectory=postgresql: systemd mints /run/postgresql/
    // owned by the unit's User/Group on every start and tears it
    // down on stop. Postgres needs this directory to drop its
    // socket lock file (.s.PGSQL.<port>.lock); without it, the
    // ProtectSystem=strict sandbox leaves /run/ read-only and
    // postgres dies with "could not create lock file ... Read-only
    // file system" before ever accepting a connection. Mode 0755
    // matches the apt-postgres package's /run/postgresql perms.
    //
    // RestartSec=5 + Restart=on-failure: kept aggressive so a
    // transient port collision recovers without operator action.
    let unit = format!(
        "[Unit]\n\
         Description=Computeza-managed PostgreSQL\n\
         Documentation=https://github.com/indritkalaj/computeza\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         User={user}\n\
         Group={user}\n\
         Environment=PGPORT={port}\n\
         RuntimeDirectory=postgresql\n\
         RuntimeDirectoryMode=0755\n\
         ExecStart={bin}/postgres -D {data} -p {port}\n\
         ExecReload=/bin/kill -HUP $MAINPID\n\
         KillMode=mixed\n\
         KillSignal=SIGINT\n\
         TimeoutSec=120\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # Hardening (spec section 10.3)\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ReadWritePaths={data}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = bin_dir.display(),
        data = data_dir.display(),
    );
    let path = PathBuf::from("/etc/systemd/system").join(unit_name);
    let mut f = fs::File::create(&path).await?;
    f.write_all(unit.as_bytes()).await?;
    f.sync_all().await?;
    info!(unit = %path.display(), "wrote systemd unit");
    Ok(path)
}

async fn wait_for_ready(host: &str, port: u16, timeout: Duration) -> Result<(), InstallError> {
    let deadline = std::time::Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            info!(%addr, "postgres is accepting connections");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(InstallError::NotReady {
                port,
                timeout_secs: timeout.as_secs(),
            });
        }
        sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_sensible() {
        let o = InstallOptions::default();
        assert_eq!(o.port, 5432);
        assert_eq!(o.system_user, "postgres");
        assert_eq!(o.unit_name, "computeza-postgres.service");
        assert_eq!(o.root_dir, PathBuf::from("/var/lib/computeza/postgres"));
    }
}
