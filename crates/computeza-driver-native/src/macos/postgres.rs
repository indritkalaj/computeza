//! Native installation of PostgreSQL on macOS via launchd.
//!
//! macOS analogue of `linux::postgres`. The pipeline:
//!
//! 1. Locate `postgres` / `initdb` (Homebrew on Apple Silicon at
//!    `/opt/homebrew/opt/postgresql@<v>/bin/`, Homebrew on Intel at
//!    `/usr/local/opt/postgresql@<v>/bin/`, or MacPorts at
//!    `/opt/local/lib/postgresql<v>/bin/`).
//! 2. Create the system-wide data directory at
//!    `/Library/Application Support/Computeza/postgres/data`.
//! 3. `initdb` as the `_postgres` system user (the macOS convention --
//!    underscore-prefixed system users).
//! 4. Write `/Library/LaunchDaemons/com.computeza.postgres.plist`.
//! 5. `launchctl bootstrap system <plist>` then `kickstart -k`.
//! 6. Wait for the daemon to accept TCP connections.
//! 7. Register `computeza-psql` via the [`crate::macos::path`] module.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, net::TcpStream, process::Command, time::sleep};
use tracing::{debug, info, warn};

use super::{launchctl, path};

/// Label that identifies the launchd service.
pub const SERVICE_LABEL: &str = "com.computeza.postgres";

/// Configuration for [`install`].
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Directory that will hold the PostgreSQL data files. Default
    /// `/Library/Application Support/Computeza/postgres`.
    pub root_dir: PathBuf,
    /// Where to find `postgres` / `initdb`. None means auto-detect.
    pub bin_dir: Option<PathBuf>,
    /// TCP port to listen on. Default 5432.
    pub port: u16,
    /// System user that owns the data directory and runs the daemon.
    /// Default `_postgres` (the macOS convention).
    pub system_user: String,
    /// Filename (under `/Library/LaunchDaemons/`) for the plist.
    pub plist_name: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/Library/Application Support/Computeza/postgres"),
            bin_dir: None,
            port: 5432,
            system_user: "_postgres".into(),
            plist_name: format!("{SERVICE_LABEL}.plist"),
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
    /// Path to the launchd plist file.
    pub plist_path: PathBuf,
    /// Port the daemon is now listening on.
    pub port: u16,
    /// Symlink created under `/usr/local/bin/` or `/opt/homebrew/bin/`.
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
    /// launchctl call failed.
    #[error(transparent)]
    Launchctl(#[from] launchctl::LaunchctlError),
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

/// Common locations a macOS Postgres install might leave its binaries.
const CANDIDATE_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/opt/postgresql@16/bin",
    "/opt/homebrew/opt/postgresql@15/bin",
    "/opt/homebrew/opt/postgresql@14/bin",
    "/usr/local/opt/postgresql@16/bin",
    "/usr/local/opt/postgresql@15/bin",
    "/usr/local/opt/postgresql@14/bin",
    "/opt/local/lib/postgresql16/bin",
    "/opt/local/lib/postgresql15/bin",
    "/Applications/Postgres.app/Contents/Versions/latest/bin",
];

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

/// Configuration for [`uninstall`].
#[derive(Clone, Debug)]
pub struct UninstallOptions {
    /// Root the install used.
    pub root_dir: PathBuf,
    /// launchd label (matches `Installed.plist_path`'s stem). Default
    /// `com.computeza.postgres`.
    pub label: String,
    /// Filename of the plist under `/Library/LaunchDaemons/`.
    pub plist_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/Library/Application Support/Computeza/postgres"),
            label: SERVICE_LABEL.into(),
            plist_name: format!("{SERVICE_LABEL}.plist"),
        }
    }
}

/// Summary returned by [`uninstall`]. Same shape as the Linux and
/// Windows variants.
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

/// Tear down a macOS PostgreSQL install written by [`install`].
///
/// Best-effort and idempotent. Mirrors the Linux/Windows shape so the
/// UI handler is OS-agnostic.
///
/// What gets removed:
/// - launchd service (bootout from system domain).
/// - `/Library/LaunchDaemons/<plist>.plist`.
/// - Data directory at `root_dir/data`.
/// - `computeza-psql` PATH symlink.
///
/// Homebrew / MacPorts-installed binaries are left alone -- v0.0.x
/// uses whatever PostgreSQL the host package manager provides.
pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, InstallError> {
    let mut out = Uninstalled::default();

    // 1. Service teardown via launchctl bootout (idempotent).
    if let Err(e) = launchctl::bootout(&opts.label).await {
        out.warn(format!("launchctl bootout system/{}: {e}", opts.label));
    } else {
        out.ok(format!("bootout system/{}", opts.label));
    }

    // 2. Remove the plist.
    let plist_path = PathBuf::from("/Library/LaunchDaemons").join(&opts.plist_name);
    if fs::try_exists(&plist_path).await.unwrap_or(false) {
        match fs::remove_file(&plist_path).await {
            Ok(()) => out.ok(format!("removed plist {}", plist_path.display())),
            Err(e) => out.warn(format!("removing plist {}: {e}", plist_path.display())),
        }
    } else {
        out.ok(format!("plist absent ({})", plist_path.display()));
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
        out.ok("removed computeza-psql symlink");
    }

    Ok(out)
}

/// Install Postgres natively on macOS.
pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    let bin_dir = match opts.bin_dir.clone() {
        Some(d) => d,
        None => detect_bin_dir().await?,
    };
    info!(bin_dir = %bin_dir.display(), "resolved postgres binaries");

    let data_dir = opts.root_dir.join("data");

    create_data_dir(&data_dir, &opts.system_user).await?;
    run_initdb_if_needed(&bin_dir, &data_dir, &opts.system_user).await?;

    let plist_path = write_launchd_plist(
        &opts.plist_name,
        &bin_dir,
        &data_dir,
        &opts.system_user,
        opts.port,
    )
    .await?;

    launchctl::bootstrap_idempotent(&plist_path.to_string_lossy()).await?;
    launchctl::kickstart(SERVICE_LABEL).await?;

    wait_for_ready("127.0.0.1", opts.port, Duration::from_secs(30)).await?;

    let psql = bin_dir.join("psql");
    let psql_symlink = match path::register("psql", &psql).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(error = %e, "registering computeza-psql symlink failed");
            None
        }
    };

    info!(port = opts.port, "postgres install complete");
    Ok(Installed {
        bin_dir,
        data_dir,
        plist_path,
        port: opts.port,
        psql_symlink,
    })
}

async fn create_data_dir(data_dir: &Path, user: &str) -> Result<(), InstallError> {
    if let Some(parent) = data_dir.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::create_dir_all(data_dir).await?;
    let parent = data_dir.parent().unwrap_or(data_dir);
    let status = Command::new("chown")
        .arg("-R")
        .arg(format!("{user}:staff"))
        .arg(parent)
        .status()
        .await?;
    if !status.success() {
        return Err(InstallError::Io(io::Error::other(format!(
            "chown -R {user}:staff {parent:?} failed"
        ))));
    }
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
    // macOS uses `sudo -u <user>` rather than runuser.
    let mut cmd = Command::new("sudo");
    cmd.arg("-u")
        .arg(user)
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

async fn write_launchd_plist(
    plist_name: &str,
    bin_dir: &Path,
    data_dir: &Path,
    user: &str,
    port: u16,
) -> Result<PathBuf, InstallError> {
    // The plist is XML; we hand-roll it because plist-rs adds a dep for
    // ~50 lines of value. Carefully encoded: paths get XML-escaped via
    // a small helper (no `<`, `>`, `&` in our paths, but defence in depth).
    let postgres_bin = bin_dir.join("postgres");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>-D</string>
        <string>{data}</string>
        <string>-p</string>
        <string>{port}</string>
    </array>
    <key>UserName</key>
    <string>{user}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/computeza-postgres.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/computeza-postgres.log</string>
</dict>
</plist>
"#,
        label = xml_escape(SERVICE_LABEL),
        bin = xml_escape(&postgres_bin.to_string_lossy()),
        data = xml_escape(&data_dir.to_string_lossy()),
        port = port,
        user = xml_escape(user),
    );
    let plist_path = PathBuf::from("/Library/LaunchDaemons").join(plist_name);
    let mut f = fs::File::create(&plist_path).await?;
    f.write_all(xml.as_bytes()).await?;
    f.sync_all().await?;
    info!(plist = %plist_path.display(), "wrote launchd plist");
    Ok(plist_path)
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
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
        assert_eq!(o.system_user, "_postgres");
        assert_eq!(o.plist_name, "com.computeza.postgres.plist");
        assert_eq!(
            o.root_dir,
            PathBuf::from("/Library/Application Support/Computeza/postgres")
        );
    }

    #[test]
    fn xml_escape_handles_all_specials() {
        assert_eq!(xml_escape("a<b>&c\"d'e"), "a&lt;b&gt;&amp;c&quot;d&apos;e");
        assert_eq!(xml_escape("plain"), "plain");
    }
}
