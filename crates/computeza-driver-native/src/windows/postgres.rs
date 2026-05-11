//! Native installation of PostgreSQL on Windows via the Service Control Manager.
//!
//! Windows analogue of `linux::postgres` / `macos::postgres`. The
//! pipeline mirrors the others structurally; differences:
//!
//! - Binaries: `C:\Program Files\PostgreSQL\<v>\bin\postgres.exe` is the
//!   standard EDB installer location.
//! - Data dir: `%PROGRAMDATA%\Computeza\postgres\data`
//!   (typically `C:\ProgramData\Computeza\postgres\data`).
//! - Service registration: `sc.exe create computeza-postgres binPath= "..."`
//!   then `sc start`. Service runs as `NT AUTHORITY\NetworkService` for
//!   v0.0.x; future iterations should use a dedicated virtual service
//!   account `NT SERVICE\computeza-postgres`.
//! - PATH: a `.cmd` shim at `C:\Program Files\Computeza\bin\computeza-psql.cmd`
//!   forwarding to the real `psql.exe`, plus a machine-PATH entry for
//!   the shim root via PowerShell.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use thiserror::Error;
use tokio::{fs, net::TcpStream, process::Command, time::sleep};
use tracing::{debug, info, warn};

use super::{path, sc};
use crate::{
    fetch::{self, Bundle, FetchError},
    progress::{InstallPhase, ProgressHandle},
};

/// Pinned PostgreSQL Windows bundle. EDB publishes the same artifact
/// for every Computeza release; we update both the URL and the SHA-256
/// in lockstep when bumping versions.
///
/// Currently tracking PostgreSQL 18.3 (the latest major as of May
/// 2026). The CI script under `scripts/check-pg-bundle.py` should be
/// run before any release to confirm the URL still resolves and to
/// refresh the pinned SHA-256.
///
/// SHA-256 is `None` for v0.0.x -- pin it before any stable release.
/// AGENTS.md tracks the audit trail when checksums change.
const PG_WINDOWS_BUNDLE: Bundle = Bundle {
    version: "18.3-1",
    url: "https://get.enterprisedb.com/postgresql/postgresql-18.3-1-windows-x64-binaries.zip",
    sha256: None,
    bin_subpath: "pgsql/bin",
};

/// Internal service name.
pub const SERVICE_NAME: &str = "computeza-postgres";
/// Display name shown in services.msc.
pub const SERVICE_DISPLAY_NAME: &str = "Computeza-managed PostgreSQL";

/// Configuration for [`install`].
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Directory that will hold the PostgreSQL data files. Default
    /// `%PROGRAMDATA%\Computeza\postgres`.
    pub root_dir: PathBuf,
    /// Where to find `postgres.exe` / `initdb.exe`. None means auto-detect.
    pub bin_dir: Option<PathBuf>,
    /// TCP port to listen on. Default 5432.
    pub port: u16,
    /// Service name to register with SCM.
    pub service_name: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        let programdata =
            std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from("C:\\ProgramData"));
        Self {
            root_dir: PathBuf::from(programdata)
                .join("Computeza")
                .join("postgres"),
            bin_dir: None,
            port: 5432,
            service_name: SERVICE_NAME.into(),
        }
    }
}

/// Information returned by a successful [`install`].
#[derive(Clone, Debug)]
pub struct Installed {
    /// Resolved binary directory.
    pub bin_dir: PathBuf,
    /// Resolved data directory.
    pub data_dir: PathBuf,
    /// Service name registered with SCM.
    pub service_name: String,
    /// Port the daemon is now listening on.
    pub port: u16,
    /// Path to the `.cmd` shim for psql.
    pub psql_shim: Option<PathBuf>,
}

/// Errors from the install pipeline.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Filesystem / process I/O.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// We could not find `postgres.exe` / `initdb.exe`.
    #[error("postgres binaries not found; tried: {0:?}")]
    BinaryNotFound(Vec<PathBuf>),
    /// `initdb.exe` failed.
    #[error("initdb failed (exit {code:?}): {stderr}")]
    InitdbFailed {
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// `pg_ctl register` failed -- this is the official PostgreSQL way
    /// to create a Windows service for a postgres data directory.
    /// Using `sc.exe create` against bare `postgres.exe` instead leads
    /// to error 1053 (service did not respond) because postgres is a
    /// console app and does not speak the SCM control protocol.
    #[error("pg_ctl register failed (exit {code:?}): {stderr}")]
    PgCtlRegisterFailed {
        /// Exit code (None means signalled).
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// SCM call failed.
    #[error(transparent)]
    Sc(#[from] sc::ScError),
    /// PATH registration failed.
    #[error(transparent)]
    Path(#[from] path::PathError),
    /// Autonomous download / extraction of the binary bundle failed.
    /// The error chain points at the EDB URL, expected vs. actual
    /// checksum, or zip-corruption details. Recovery: re-run when the
    /// network is reachable, or pre-install PostgreSQL manually and
    /// pass `--bin_dir` (CLI) / set `InstallOptions::bin_dir` so the
    /// installer skips the download path.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// Server never started accepting connections.
    #[error("postgres did not become ready on port {port} within {timeout_secs}s")]
    NotReady {
        /// Port we were waiting on.
        port: u16,
        /// How long we waited.
        timeout_secs: u64,
    },
}

/// Common locations a Windows Postgres install might leave its binaries.
/// We also check our own cache root (populated by `fetch::fetch_and_extract`)
/// as the last fallback, before deciding to download.
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "C:\\Program Files\\PostgreSQL\\18\\bin",
    "C:\\Program Files\\PostgreSQL\\17\\bin",
    "C:\\Program Files\\PostgreSQL\\16\\bin",
    "C:\\Program Files\\PostgreSQL\\15\\bin",
    "C:\\Program Files\\PostgreSQL\\14\\bin",
    "C:\\Program Files\\PostgreSQL\\13\\bin",
];

async fn detect_bin_dir() -> Result<PathBuf, Vec<PathBuf>> {
    let mut tried = Vec::new();
    for c in CANDIDATE_BIN_DIRS {
        let dir = PathBuf::from(c);
        tried.push(dir.clone());
        if fs::try_exists(dir.join("postgres.exe"))
            .await
            .unwrap_or(false)
            && fs::try_exists(dir.join("initdb.exe"))
                .await
                .unwrap_or(false)
        {
            return Ok(dir);
        }
    }
    Err(tried)
}

/// Resolve the binary directory: caller override > host-installed EDB
/// release > Computeza-managed cache > autonomous download from EDB.
///
/// The last leg implements the user mandate that the installer "should
/// install the components automatically". If `opts.bin_dir` is set or
/// a Program Files install is found, we never touch the network.
async fn ensure_bin_dir(
    opts: &InstallOptions,
    progress: &ProgressHandle,
) -> Result<PathBuf, InstallError> {
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Looking for an existing PostgreSQL install");
    if let Some(d) = &opts.bin_dir {
        info!(bin_dir = %d.display(), "using caller-supplied bin_dir");
        progress.set_message(format!("Using {}", d.display()));
        return Ok(d.clone());
    }
    match detect_bin_dir().await {
        Ok(d) => {
            info!(bin_dir = %d.display(), "detected existing host PostgreSQL install");
            progress.set_message(format!("Found {}", d.display()));
            Ok(d)
        }
        Err(tried) => {
            info!(
                tried = ?tried,
                "no host PostgreSQL install found; falling through to managed bundle download. \
                 To skip this step pre-install PostgreSQL or pass InstallOptions::bin_dir."
            );
            progress.set_message(
                "No host PostgreSQL detected; downloading bundled PostgreSQL from EDB",
            );
            let cache_root = opts.root_dir.join("binaries");
            let bin = fetch::fetch_and_extract(&cache_root, &PG_WINDOWS_BUNDLE, progress).await?;
            // Sanity-check: the binary we expect must actually exist
            // after extraction. If not, the bundle layout has changed
            // and we need to update `bin_subpath`.
            if !fs::try_exists(bin.join("postgres.exe"))
                .await
                .unwrap_or(false)
            {
                return Err(InstallError::BinaryNotFound(vec![bin]));
            }
            Ok(bin)
        }
    }
}

/// Install Postgres natively on Windows without progress reporting.
pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    install_with_progress(opts, &ProgressHandle::noop()).await
}

/// Install Postgres natively on Windows while reporting progress
/// through `progress`. The CLI uses `install` (no progress); the UI
/// server uses this entry point so the wizard can render a live bar.
pub async fn install_with_progress(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<Installed, InstallError> {
    let bin_dir = ensure_bin_dir(&opts, progress).await?;
    info!(bin_dir = %bin_dir.display(), "resolved postgres binaries");

    let data_dir = opts.root_dir.join("data");

    progress.set_phase(InstallPhase::Initdb);
    progress.set_message(format!("Initializing data dir at {}", data_dir.display()));
    create_data_dir(&data_dir).await?;
    run_initdb_if_needed(&bin_dir, &data_dir).await?;

    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering Windows service {}", opts.service_name));
    register_service(&bin_dir, &data_dir, &opts.service_name, opts.port).await?;
    progress.set_phase(InstallPhase::StartingService);
    progress.set_message("Starting service");
    sc::start(&opts.service_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {} to accept connections",
        opts.port
    ));
    wait_for_ready("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

    progress.set_phase(InstallPhase::RegisteringPath);
    progress.set_message("Registering psql in PATH");
    let psql_exe = bin_dir.join("psql.exe");
    let psql_shim = match path::register("psql", &psql_exe).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(error = %e, "registering computeza-psql.cmd shim failed");
            None
        }
    };

    info!(port = opts.port, "postgres install complete");
    Ok(Installed {
        bin_dir,
        data_dir,
        service_name: opts.service_name,
        port: opts.port,
        psql_shim,
    })
}

async fn create_data_dir(data_dir: &Path) -> Result<(), InstallError> {
    if let Some(parent) = data_dir.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::create_dir_all(data_dir).await?;
    // On Windows we rely on Program-Files / ProgramData ACL inheritance
    // -- no chmod equivalent. The service account (NetworkService) gains
    // read-write via the ProgramData ACL.
    Ok(())
}

/// Register a Windows service that runs `postgres.exe` against the
/// given data dir. Uses `pg_ctl register` -- the only Postgres-aware
/// way to register the service.
///
/// `postgres.exe` itself is a console application, not a service
/// binary. Registering it directly with `sc.exe create` (the previous
/// approach) yields error 1053 ("service did not respond to the start
/// or control request in a timely fashion") because postgres has no
/// SCM handler. `pg_ctl register` writes a service entry that uses
/// pg_ctl's own service wrapper, which translates SCM commands into
/// pg_ctl start/stop calls.
///
/// Idempotency: if a service of the same name exists (from a prior
/// failed run), we stop + delete it first so the register call gets a
/// clean slate.
async fn register_service(
    bin_dir: &Path,
    data_dir: &Path,
    service_name: &str,
    port: u16,
) -> Result<(), InstallError> {
    // Wipe any leftover broken service from a previous attempt. Both
    // sc::stop and sc::delete are idempotent (they swallow "service
    // does not exist" / "service not running").
    let _ = sc::stop(service_name).await;
    let _ = sc::delete(service_name).await;

    let pg_ctl = bin_dir.join("pg_ctl.exe");
    info!(pg_ctl = %pg_ctl.display(), service = %service_name, "registering service via pg_ctl");
    let out = Command::new(&pg_ctl)
        .arg("register")
        .arg("-N")
        .arg(service_name)
        .arg("-D")
        .arg(data_dir)
        .arg("-S")
        .arg("auto")
        .arg("-w")
        .arg("-o")
        .arg(format!("-p {port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        return Err(InstallError::PgCtlRegisterFailed {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

async fn run_initdb_if_needed(bin_dir: &Path, data_dir: &Path) -> Result<(), InstallError> {
    let marker = data_dir.join("PG_VERSION");
    if fs::try_exists(&marker).await? {
        debug!(data_dir = %data_dir.display(), "data dir already initialised; skipping initdb");
        return Ok(());
    }
    info!(data_dir = %data_dir.display(), "running initdb");
    let mut cmd = Command::new(bin_dir.join("initdb.exe"));
    cmd.arg("-D")
        .arg(data_dir)
        .arg("--auth-host=scram-sha-256")
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
        assert_eq!(o.service_name, "computeza-postgres");
        assert!(
            o.root_dir
                .to_string_lossy()
                .to_lowercase()
                .ends_with("computeza\\postgres"),
            "unexpected root_dir: {:?}",
            o.root_dir
        );
    }
}
