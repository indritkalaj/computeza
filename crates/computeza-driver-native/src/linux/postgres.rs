//! Native installation of PostgreSQL on Linux via systemd.
//!
//! This is the first end-to-end demonstration of the autonomous-installer
//! mandate (spec section 2.1). The installer:
//!
//! 1. Locates the `postgres` and `initdb` binaries shipped by the system
//!    package (typical paths: `/usr/lib/postgresql/<v>/bin/`, `/usr/bin/`).
//!    v0.0.x assumes the package is present -- a follow-up will download,
//!    SHA-verify, and extract a vendored binary tarball so even this
//!    dependency goes away.
//! 2. Creates a `postgres`-owned data directory at
//!    `/var/lib/computeza/postgres/data` and runs `initdb` against it
//!    (idempotent: skipped if the directory is already initialised).
//! 3. Writes a systemd unit at
//!    `/etc/systemd/system/computeza-postgres.service` and reloads the
//!    systemd manager.
//! 4. `systemctl enable --now computeza-postgres.service`.
//! 5. Waits until Postgres accepts TCP connections on the configured port.
//! 6. Registers a `computeza-psql` symlink under `/usr/local/bin/` per
//!    AGENTS.md rule section 4 (cross-platform PATH registration).
//!
//! Privileged operations (writing under `/var/lib`, `/etc/systemd/system`,
//! `/usr/local/bin`, running `initdb` as the postgres user, `systemctl`)
//! mean this code expects to be invoked while the binary is running as
//! root. The wrapping `computeza install` subcommand re-execs itself
//! with `sudo` when not already root.

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

/// Configuration for [`install`].
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Directory that will hold the PostgreSQL data files. Default
    /// `/var/lib/computeza/postgres`. The actual data lives in a `data/`
    /// subdirectory; everything else (`log/`, `run/`) is alongside.
    pub root_dir: PathBuf,
    /// Where to find the `postgres` / `initdb` binaries. None means
    /// auto-detect by scanning common locations.
    pub bin_dir: Option<PathBuf>,
    /// TCP port to listen on. Default 5432.
    pub port: u16,
    /// System user that owns the data directory and runs the daemon.
    /// Default `postgres`.
    pub system_user: String,
    /// Name of the systemd unit. Default `computeza-postgres.service`.
    pub unit_name: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/postgres"),
            bin_dir: None,
            port: 5432,
            system_user: "postgres".into(),
            unit_name: "computeza-postgres.service".into(),
        }
    }
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
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "/usr/lib/postgresql/16/bin",
    "/usr/lib/postgresql/15/bin",
    "/usr/lib/postgresql/14/bin",
    "/usr/pgsql-16/bin",
    "/usr/pgsql-15/bin",
    "/opt/postgresql/bin",
    "/usr/bin",
    "/usr/local/bin",
];

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

/// Install Postgres natively.
pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    let bin_dir = match opts.bin_dir.clone() {
        Some(d) => d,
        None => detect_bin_dir().await?,
    };
    info!(bin_dir = %bin_dir.display(), "resolved postgres binaries");

    let data_dir = opts.root_dir.join("data");

    create_data_dir(&data_dir, &opts.system_user).await?;
    run_initdb_if_needed(&bin_dir, &data_dir, &opts.system_user).await?;

    let unit_path = write_systemd_unit(
        &opts.unit_name,
        &bin_dir,
        &data_dir,
        &opts.system_user,
        opts.port,
    )
    .await?;

    systemctl::daemon_reload().await?;
    systemctl::enable_now(&opts.unit_name).await?;

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
