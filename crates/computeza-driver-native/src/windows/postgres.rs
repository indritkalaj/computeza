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
    /// SCM call failed.
    #[error(transparent)]
    Sc(#[from] sc::ScError),
    /// PATH registration failed.
    #[error(transparent)]
    Path(#[from] path::PathError),
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
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "C:\\Program Files\\PostgreSQL\\16\\bin",
    "C:\\Program Files\\PostgreSQL\\15\\bin",
    "C:\\Program Files\\PostgreSQL\\14\\bin",
    "C:\\Program Files\\PostgreSQL\\13\\bin",
];

async fn detect_bin_dir() -> Result<PathBuf, InstallError> {
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
    Err(InstallError::BinaryNotFound(tried))
}

/// Install Postgres natively on Windows.
pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    let bin_dir = match opts.bin_dir.clone() {
        Some(d) => d,
        None => detect_bin_dir().await?,
    };
    info!(bin_dir = %bin_dir.display(), "resolved postgres binaries");

    let data_dir = opts.root_dir.join("data");

    create_data_dir(&data_dir).await?;
    run_initdb_if_needed(&bin_dir, &data_dir).await?;

    // bin_path for the service: quote the exe path, then -D <data>, -p <port>.
    let postgres_exe = bin_dir.join("postgres.exe");
    let bin_path = format!(
        "\"{}\" -D \"{}\" -p {}",
        postgres_exe.display(),
        data_dir.display(),
        opts.port
    );
    sc::create(&sc::ServiceSpec {
        name: &opts.service_name,
        display_name: SERVICE_DISPLAY_NAME,
        bin_path: &bin_path,
        start: "auto",
    })
    .await?;
    sc::start(&opts.service_name).await?;

    wait_for_ready("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

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
