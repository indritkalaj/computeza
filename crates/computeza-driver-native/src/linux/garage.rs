//! Garage S3-compatible object storage. Linux install path.
//!
//! Garage's distribution story has shifted over time. The project
//! used to publish pre-built `x86_64-unknown-linux-musl/garage`
//! binaries on `garagehq.deuxfleurs.fr/_releases/`, but those URLs
//! aren't kept current for every tag. The canonical source is
//! Deuxfleurs' self-hosted Gitea at `git.deuxfleurs.fr` -- they tag
//! every release there and the archive endpoint serves a tarball of
//! the source tree.
//!
//! So this driver mirrors the kanidm pattern: source-build from an
//! upstream-pinned tag rather than depend on a pre-built binary.
//!
//! Flow on a fresh host:
//!
//! 1. Resolve `cargo` (auto-install Rust toolchain into
//!    `/var/lib/computeza/toolchain/rust` if missing).
//! 2. Download `git.deuxfleurs.fr/Deuxfleurs/garage/archive/v<version>.tar.gz`,
//!    extract under `<root_dir>/src/garage-<version>/`. Cached:
//!    re-runs hit the existing extraction.
//! 3. `cargo build --release` against the unpacked source. Slow
//!    (15-30 minutes on a 4-core box); progress messages communicate
//!    this. The build output lands at `<root_dir>/src/.../target/release/garage`.
//! 4. Copy the binary into `<root_dir>/bin/garage` so the rest of
//!    the pipeline (systemd unit, PATH shim) targets a stable path.
//! 5. Write `garage.toml` + systemd unit, daemon-reload, enable --now.
//! 6. Wait for the S3 API port.

use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::info;

use crate::progress::{InstallPhase, ProgressHandle};

use super::service::{InstalledService, ServiceError, Uninstalled};

pub const UNIT_NAME: &str = "computeza-garage.service";
pub const DEFAULT_S3_PORT: u16 = 3900;

/// Pinned Garage versions. These map to git tags on
/// `git.deuxfleurs.fr/Deuxfleurs/garage`. First entry is the
/// default ("latest"). The driver prepends `v` to form the tag.
///
/// When bumping: verify the tag exists at
/// <https://git.deuxfleurs.fr/Deuxfleurs/garage/tags> and that the
/// build still compiles against the Rust toolchain that
/// `prerequisites::ensure_rust_toolchain` installs.
const GARAGE_VERSIONS: &[&str] = &["2.3.0", "1.1.0"];

/// Upstream source archive endpoint. The Gitea instance serves
/// `archive/v<tag>.tar.gz` for every tag; the resulting tarball
/// unpacks to a `garage-<tag>/` directory.
const GARAGE_ARCHIVE_BASE: &str = "https://git.deuxfleurs.fr/Deuxfleurs/garage/archive";

#[must_use]
pub fn available_versions() -> &'static [&'static str] {
    GARAGE_VERSIONS
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub unit_name: String,
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/garage"),
            port: DEFAULT_S3_PORT,
            unit_name: UNIT_NAME.into(),
            version: None,
        }
    }
}

pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    let version = opts.version.as_deref().unwrap_or(GARAGE_VERSIONS[0]);
    fs::create_dir_all(&opts.root_dir).await?;

    // 1. Toolchain resolution (matches the kanidm flow).
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Ensuring Rust toolchain is installed for the garage source build");
    let cargo_path = crate::prerequisites::ensure_rust_toolchain(progress).await?;

    // 2. Fetch + extract the source tarball.
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!(
        "Downloading Garage v{version} source tarball from git.deuxfleurs.fr (one-time)"
    ));
    let src_root = opts.root_dir.join("src");
    fs::create_dir_all(&src_root).await?;
    let src_dir = ensure_source_extracted(&src_root, version, progress).await?;

    // 3. cargo build --release.
    progress.set_phase(InstallPhase::Extracting);
    progress.set_message(format!(
        "cargo build --release Garage v{version} -- 15-30 min on a 4-core host"
    ));
    cargo_build_release(&cargo_path, &src_dir).await?;

    // 4. Stable binary path under <root>/bin/garage.
    let bin_dir = opts.root_dir.join("bin");
    fs::create_dir_all(&bin_dir).await?;
    let built = src_dir.join("target").join("release").join("garage");
    if !fs::try_exists(&built).await.unwrap_or(false) {
        return Err(ServiceError::BinaryMissing {
            binary: "garage".into(),
            bin_dir: src_dir.join("target").join("release"),
        });
    }
    let final_binary = bin_dir.join("garage");

    // Stop the service before replacing the binary so a re-install
    // (where garage is already running) does not hit `Text file
    // busy` (ETXTBSY) on the copy. Best-effort: on a fresh install
    // the unit doesn't exist yet, so `systemctl stop` returns
    // non-zero harmlessly.
    let _ = super::systemctl::stop(&opts.unit_name).await;

    // Atomic replace via tmp + rename. rename() on Linux works even
    // if the target is currently executing -- the kernel re-points
    // the directory entry to a new inode rather than overwriting
    // the existing one in place.
    let tmp_binary = bin_dir.join("garage.new");
    if fs::try_exists(&tmp_binary).await.unwrap_or(false) {
        let _ = fs::remove_file(&tmp_binary).await;
    }
    fs::copy(&built, &tmp_binary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = tokio::fs::set_permissions(&tmp_binary, perms).await;
    }
    fs::rename(&tmp_binary, &final_binary).await?;

    // 5. Config + systemd unit.
    let s3_port = opts.port;
    let rpc_port = s3_port + 1;
    let web_port = s3_port + 2;
    let admin_port = s3_port + 3;
    let config_path = opts.root_dir.join("garage.toml");
    let config = garage_toml(&opts.root_dir, s3_port, rpc_port, web_port, admin_port);
    let mut f = fs::File::create(&config_path).await?;
    f.write_all(config.as_bytes()).await?;
    f.sync_all().await?;
    info!(config = %config_path.display(), "wrote garage.toml");

    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit = systemd_unit(&final_binary, &config_path, &opts.root_dir);
    let mut f = fs::File::create(&unit_path).await?;
    f.write_all(unit.as_bytes()).await?;
    f.sync_all().await?;
    info!(unit = %unit_path.display(), "wrote garage systemd unit");
    super::systemctl::daemon_reload().await?;

    // 6. Start + wait.
    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    super::systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!(
        "Waiting for port {s3_port} to accept S3 API connections"
    ));
    wait_for_port("127.0.0.1", s3_port, std::time::Duration::from_secs(60)).await?;

    // PATH shim.
    let cli_symlink = match super::path::register("garage", &final_binary).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "registering garage symlink failed; non-fatal");
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
            root_dir: PathBuf::from("/var/lib/computeza/garage"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    super::service::uninstall_service("garage", &opts.root_dir, &opts.unit_name, Some("garage"))
        .await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/garage");
    if !fs::try_exists(root.join("data")).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-garage".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_S3_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: Some(root.join("bin")),
    }]
}

/// Download + extract the upstream source tarball under
/// `<src_root>/`. Cached: if any subdirectory under `src_root`
/// already contains a `Cargo.toml`, returns that path without
/// re-downloading. Failure to extract surfaces as a `ServiceError`.
///
/// We don't assume a specific top-level directory name inside the
/// tarball because Gitea, GitHub, and GitLab all use different
/// conventions:
///   - GitHub: `<repo>-<version-without-v>/`
///   - Gitea: `<repo>/` (no version suffix)
///   - GitLab: `<repo>-<sha>-<version>/`
///
/// Instead, we list `src_root`'s direct children after extraction
/// and pick whichever one has a Cargo.toml at the top.
async fn ensure_source_extracted(
    src_root: &Path,
    version: &str,
    _progress: &ProgressHandle,
) -> Result<PathBuf, ServiceError> {
    if let Some(existing) = find_cargo_root(src_root).await {
        info!(
            extracted = %existing.display(),
            "garage source already extracted; skipping download"
        );
        return Ok(existing);
    }

    let url = format!("{GARAGE_ARCHIVE_BASE}/v{version}.tar.gz");
    let tarball = src_root.join(format!("garage-{version}.tar.gz"));
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("GET {url}: {e}"))))?;
    if !resp.status().is_success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "GET {url} returned HTTP {}; verify the tag exists at https://git.deuxfleurs.fr/Deuxfleurs/garage/tags",
            resp.status().as_u16()
        ))));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("reading body: {e}"))))?;
    let mut f = fs::File::create(&tarball).await?;
    f.write_all(&bytes).await?;
    f.sync_all().await?;
    drop(f);

    // Extract via tar -- shell out rather than pull in another
    // archive crate. `tar` is in coreutils everywhere we run.
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(src_root)
        .status()
        .await?;
    if !status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "tar -xzf {} failed (exit {:?}); the downloaded archive may be incomplete or the tag may not exist",
            tarball.display(),
            status.code()
        ))));
    }
    // Best-effort cleanup of the tarball after extraction.
    let _ = fs::remove_file(&tarball).await;

    find_cargo_root(src_root).await.ok_or_else(|| {
        ServiceError::Io(std::io::Error::other(format!(
            "extracted under {} but no subdirectory contains a Cargo.toml at its root. The downloaded archive may be incomplete, or the tag may use a non-standard layout. Run `ls {}` to inspect what was unpacked.",
            src_root.display(),
            src_root.display()
        )))
    })
}

/// Scan `src_root`'s direct children for the first subdirectory
/// that has a Cargo.toml at its root. Returns `None` if no such
/// directory exists. Used to be tolerant of Gitea / GitHub /
/// GitLab archive naming differences without hardcoding the
/// expected layout per source.
async fn find_cargo_root(src_root: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(src_root).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !entry.file_type().await.ok()?.is_dir() {
            continue;
        }
        if fs::try_exists(path.join("Cargo.toml"))
            .await
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

/// Run `cargo build --release` against the unpacked source tree.
/// Surfaces stderr verbatim on failure so the operator sees which
/// crate failed to compile.
async fn cargo_build_release(cargo_bin: &Path, src_dir: &Path) -> Result<(), ServiceError> {
    let out = Command::new(cargo_bin)
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg("garage")
        .arg("--manifest-path")
        .arg(src_dir.join("Cargo.toml"))
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "cargo build --release for garage failed (cargo={}, src={}). Full stderr:\n{}",
            cargo_bin.display(),
            src_dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ))));
    }
    Ok(())
}

fn garage_toml(
    root_dir: &Path,
    s3_port: u16,
    rpc_port: u16,
    web_port: u16,
    admin_port: u16,
) -> String {
    format!(
        "metadata_dir = \"{root}/data/meta\"\n\
         data_dir = \"{root}/data/data\"\n\
         db_engine = \"sqlite\"\n\
         replication_factor = 1\n\
         rpc_bind_addr = \"127.0.0.1:{rpc_port}\"\n\
         rpc_public_addr = \"127.0.0.1:{rpc_port}\"\n\
         rpc_secret = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
         \n\
         [s3_api]\n\
         api_bind_addr = \"127.0.0.1:{s3_port}\"\n\
         s3_region = \"garage\"\n\
         root_domain = \".s3.garage.local\"\n\
         \n\
         [s3_web]\n\
         bind_addr = \"127.0.0.1:{web_port}\"\n\
         root_domain = \".web.garage.local\"\n\
         index = \"index.html\"\n\
         \n\
         [admin]\n\
         api_bind_addr = \"127.0.0.1:{admin_port}\"\n\
         admin_token = \"change-me\"\n\
         metrics_token = \"change-me\"\n",
        root = root_dir.display(),
    )
}

fn systemd_unit(binary: &Path, config: &Path, root_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Computeza-managed Garage (S3-compatible object storage)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         RuntimeDirectory=garage\n\
         RuntimeDirectoryMode=0755\n\
         ExecStart={bin} -c {conf} server\n\
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
        bin = binary.display(),
        conf = config.display(),
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
