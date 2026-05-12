//! Kanidm install path on Linux. Built on `linux::service` for the
//! service-registration tail, but the binary-acquisition phase uses
//! `cargo install` instead of `fetch::Bundle` because kanidm does not
//! publish prebuilt binaries on GitHub releases. See the AGENTS.md
//! "Verify the distribution channel BEFORE writing a driver" note for
//! the lesson that drove this design.
//!
//! Flow on a fresh host:
//!
//! 1. Verify `cargo` is on PATH.
//! 2. `cargo install kanidmd --locked --version <X> --root <root>`.
//!    Slow (10-15 min compile on a 4-core box); progress messages
//!    communicate this.
//! 3. Generate a self-signed TLS cert via openssl shell-out --
//!    kanidm refuses to start without TLS even on loopback.
//! 4. Write `server.toml` referencing the cert paths.
//! 5. Write systemd unit, daemon-reload, enable --now.
//! 6. Wait for the TCP port.
//!
//! Operator follow-up after install completes: kanidm requires
//! recovering the initial admin password via
//! `kanidmd recover_account admin` (run as root). The driver does
//! not automate this -- the password lands on stdout and the
//! operator stores it via their secrets workflow.

use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::info;

use crate::progress::{InstallPhase, ProgressHandle};

use super::service::{self, InstalledService, ServiceError, Uninstalled};

pub const SERVICE_NAME: &str = "computeza-kanidm";
pub const UNIT_NAME: &str = "computeza-kanidm.service";
pub const DEFAULT_PORT: u16 = 8443;

/// Pinned kanidm versions. These map to crates.io versions of the
/// `kanidmd` crate. First entry is the default ("latest").
const KANIDM_VERSIONS: &[&str] = &["1.10.1", "1.9.3"];

pub fn available_versions() -> &'static [&'static str] {
    KANIDM_VERSIONS
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub unit_name: String,
    /// Version string from the dropdown (e.g. "1.10.1"). `None`
    /// resolves to `KANIDM_VERSIONS[0]`.
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/kanidm"),
            port: DEFAULT_PORT,
            unit_name: UNIT_NAME.into(),
            version: None,
        }
    }
}

pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    let version = opts.version.as_deref().unwrap_or(KANIDM_VERSIONS[0]);

    // 1. Toolchain probe.
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Verifying cargo + openssl are on PATH");
    require_on_path("cargo").await?;
    require_on_path("openssl").await?;

    // 2. cargo install kanidmd.
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!(
        "cargo install kanidmd@{version} -- compiles from source, takes 10-15 minutes"
    ));
    fs::create_dir_all(&opts.root_dir).await?;
    cargo_install_kanidmd(&opts.root_dir, version).await?;
    let bin_dir = opts.root_dir.join("bin");
    if !fs::try_exists(bin_dir.join("kanidmd"))
        .await
        .unwrap_or(false)
    {
        return Err(ServiceError::BinaryMissing {
            binary: "kanidmd".into(),
            bin_dir: bin_dir.clone(),
        });
    }

    // 3. Self-signed TLS cert (kanidm requires TLS even on loopback).
    progress.set_phase(InstallPhase::Initdb);
    progress.set_message("Generating self-signed TLS cert via openssl");
    let data_dir = opts.root_dir.join("data");
    fs::create_dir_all(&data_dir).await?;
    let cert_path = opts.root_dir.join("cert.pem");
    let key_path = opts.root_dir.join("key.pem");
    if !fs::try_exists(&cert_path).await.unwrap_or(false) {
        generate_self_signed(&cert_path, &key_path).await?;
    }

    // 4. server.toml.
    let server_toml = kanidm_server_toml(&opts.root_dir, opts.port, &cert_path, &key_path);
    let server_toml_path = opts.root_dir.join("server.toml");
    fs::write(&server_toml_path, server_toml).await?;

    // 5. systemd unit + start, reusing the service helper's tail.
    //    We can't call `service::install_service` directly because it
    //    expects a Bundle to fetch -- the cargo path replaces that
    //    phase. Reimplement just the systemd registration here.
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit_body = systemd_unit(&bin_dir, &server_toml_path, &opts.root_dir);
    let mut f = fs::File::create(&unit_path).await?;
    f.write_all(unit_body.as_bytes()).await?;
    f.sync_all().await?;
    info!(unit = %unit_path.display(), "wrote systemd unit");
    super::systemctl::daemon_reload().await?;

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    super::systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {} to accept connections",
        opts.port
    ));
    wait_for_port("127.0.0.1", opts.port, std::time::Duration::from_secs(60)).await?;

    // 6. PATH shim for the kanidmd binary (no CLI tools shipped via
    //    this install path; operators install `kanidm_tools`
    //    separately if they want the client).
    progress.set_phase(InstallPhase::RegisteringPath);
    let cli_symlink = match super::path::register("kanidmd", &bin_dir.join("kanidmd")).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "registering kanidmd symlink failed; non-fatal");
            None
        }
    };

    Ok(InstalledService {
        bin_dir,
        unit_path,
        port: opts.port,
        cli_symlink,
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
            root_dir: PathBuf::from("/var/lib/computeza/kanidm"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("kanidm", &opts.root_dir, &opts.unit_name, Some("kanidmd")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/kanidm");
    if !fs::try_exists(root.join("bin").join("kanidmd"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-kanidm".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: Some(root.join("bin")),
    }]
}

async fn require_on_path(cmd: &str) -> Result<(), ServiceError> {
    let out = Command::new("which").arg(cmd).output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::Io(std::io::Error::other(format!(
            "`{cmd}` not found on PATH. Kanidm installs via `cargo install` from source; the host needs both `cargo` (Rust toolchain) and `openssl` (TLS cert generation) before this install can run. On Debian/Ubuntu: `apt install cargo openssl`. On Fedora: `dnf install cargo openssl`."
        ))))
    }
}

async fn cargo_install_kanidmd(root_dir: &Path, version: &str) -> Result<(), ServiceError> {
    let out = Command::new("cargo")
        .arg("install")
        .arg("kanidmd")
        .arg("--locked")
        .arg("--version")
        .arg(version)
        .arg("--root")
        .arg(root_dir)
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "cargo install kanidmd@{version} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))));
    }
    Ok(())
}

async fn generate_self_signed(cert: &Path, key: &Path) -> Result<(), ServiceError> {
    let out = Command::new("openssl")
        .arg("req")
        .arg("-x509")
        .arg("-newkey")
        .arg("rsa:2048")
        .arg("-nodes")
        .arg("-keyout")
        .arg(key)
        .arg("-out")
        .arg(cert)
        .arg("-days")
        .arg("365")
        .arg("-subj")
        .arg("/CN=localhost")
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "openssl self-signed cert generation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))));
    }
    Ok(())
}

fn kanidm_server_toml(root_dir: &Path, port: u16, cert: &Path, key: &Path) -> String {
    format!(
        "bindaddress = \"127.0.0.1:{port}\"\n\
         domain = \"localhost\"\n\
         origin = \"https://localhost:{port}\"\n\
         db_path = \"{root}/data/kanidm.db\"\n\
         role = \"WriteReplica\"\n\
         tls_chain = \"{cert}\"\n\
         tls_key   = \"{key}\"\n",
        root = root_dir.display(),
        cert = cert.display(),
        key = key.display(),
    )
}

fn systemd_unit(bin_dir: &Path, config_path: &Path, root_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Computeza-managed Kanidm\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin}/kanidmd server -c {conf}\n\
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
        bin = bin_dir.display(),
        conf = config_path.display(),
        root = root_dir.display(),
    )
}

async fn wait_for_port(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
) -> Result<(), ServiceError> {
    let deadline = std::time::Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    loop {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ServiceError::NotReady {
                port,
                timeout_secs: timeout.as_secs(),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
