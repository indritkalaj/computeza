//! Kanidm install path on Linux. Built on `linux::service` for the
//! service-registration tail, but the binary-acquisition phase
//! source-builds from the upstream GitHub tarball because kanidm
//! does not publish prebuilt binaries on GitHub releases AND its
//! crates.io presence is patchy. See the AGENTS.md "Verify the
//! distribution channel BEFORE writing a driver" note for the
//! lesson that drove this design.
//!
//! Flow on a fresh host:
//!
//! 1. Resolve `cargo` -- use it from `$PATH` if present, otherwise
//!    install the toolchain via `prerequisites::ensure_rust_toolchain`
//!    (system-wide into `/var/lib/computeza/toolchain/rust` + symlinks
//!    on `/usr/local/bin`).
//! 2. Download `github.com/kanidm/kanidm/archive/refs/tags/v<X>.tar.gz`,
//!    extract under `<root>/src/kanidm-<X>/` (cached on re-run).
//! 3. `cargo build --release --bin kanidmd --manifest-path <src>/Cargo.toml`.
//!    `--bin kanidmd` selects the binary by NAME (the binary name
//!    has been stable across releases) rather than by package name
//!    (which has churned across workspace refactors and broke
//!    `cargo install kanidmd` on recent tags). Slow (10-20 min
//!    compile on a 4-core box); progress messages communicate this.
//! 4. Copy the resulting binary to `<root>/bin/kanidmd` so the
//!    rest of the pipeline targets a stable path.
//! 5. Generate a self-signed TLS cert in pure Rust via `rcgen` --
//!    kanidm refuses to start without TLS even on loopback. No host
//!    openssl dependency.
//! 6. Write `server.toml` referencing the cert paths.
//! 7. Write systemd unit, daemon-reload, enable --now.
//! 8. Wait for the TCP port.
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

use super::release::{self, ReleaseManifest};
use super::service::{self, InstalledService, ServiceError, Uninstalled};

/// How many old release directories to retain after a successful
/// install. Three is the sweet spot: enough for "rollback to the
/// version before last" without burning unbounded disk. The
/// active release is always preserved regardless.
const KANIDM_RELEASE_RETENTION: usize = 3;

pub const SERVICE_NAME: &str = "computeza-kanidm";
pub const UNIT_NAME: &str = "computeza-kanidm.service";
pub const DEFAULT_PORT: u16 = 8443;

/// Pinned kanidm versions. These map to **git tags** on
/// `github.com/kanidm/kanidm`; the driver fetches the matching
/// source tarball from GitHub's archive endpoint. First entry is
/// the default ("latest"). The version string is used twice: as
/// the tag (prefixed `v`) and as the unpacked directory suffix
/// (GitHub names archive directories `kanidm-<version>/`).
///
/// When bumping: verify the tag exists at
/// <https://github.com/kanidm/kanidm/tags> AND that the build still
/// compiles against the toolchain `prerequisites::ensure_rust_toolchain`
/// installs (kanidm tracks the latest stable Rust closely).
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

    // 1. Toolchain resolution. cargo is rare on production hosts;
    //    rather than surface "missing cargo" as an error and force the
    //    operator to run rustup themselves, we install a shared Rust
    //    toolchain into /var/lib/computeza/toolchain/rust and symlink
    //    cargo / rustc / rustup onto /usr/local/bin so the operator's
    //    shell (and any subsequent install) sees them on $PATH
    //    transparently. openssl used to be a hard prereq here too
    //    (for the TLS cert step below); it's now generated in pure
    //    Rust via rcgen, so no host openssl is needed.
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Ensuring Rust toolchain is installed");
    fs::create_dir_all(&opts.root_dir).await?;
    let cargo_path = crate::prerequisites::ensure_rust_toolchain(progress).await?;

    // 2. Build kanidmd from the upstream source tarball.
    //
    //    Why not `cargo install --git --tag kanidmd`? Because kanidm
    //    restructured its workspace and the binary crate's
    //    *package* name no longer matches the binary name `kanidmd`.
    //    `cargo install` requires a package name; `cargo build
    //    --bin kanidmd` selects a binary by name regardless of
    //    which workspace member owns it, which is what we want.
    //
    //    Flow mirrors the garage driver: fetch the GitHub source
    //    tarball, extract under <root>/src/kanidm-<version>/,
    //    cargo build --release --bin kanidmd, copy the resulting
    //    binary into <root>/bin/kanidmd.
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!(
        "Downloading Kanidm v{version} source tarball from github.com (one-time)"
    ));
    let src_root = opts.root_dir.join("src");
    fs::create_dir_all(&src_root).await?;
    let src_dir = ensure_kanidm_source_extracted(&src_root, version).await?;

    progress.set_phase(InstallPhase::Extracting);
    progress.set_message(format!(
        "cargo build --release --bin kanidmd v{version} -- compiles from source, takes 10-20 minutes"
    ));
    cargo_build_kanidmd(&cargo_path, &src_dir).await?;

    let built = src_dir.join("target").join("release").join("kanidmd");
    if !fs::try_exists(&built).await.unwrap_or(false) {
        return Err(ServiceError::BinaryMissing {
            binary: "kanidmd".into(),
            bin_dir: src_dir.join("target").join("release"),
        });
    }

    // Lay the freshly-built binary into a fresh release directory
    // and atomic-swap `<root>/current` to point at it. The systemd
    // unit's ExecStart and WorkingDirectory both reference
    // `<root>/current/...`, so the swap is what actually upgrades
    // the running daemon -- never an in-place overwrite of the
    // executing binary.
    progress.set_message(format!(
        "Staging kanidm v{version} into a fresh release directory"
    ));
    let new_release = release::new_release(&opts.root_dir, version)
        .await
        .map_err(release_error_to_service)?;
    let release_binary = new_release.dir.join("kanidmd");
    fs::copy(&built, &release_binary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = tokio::fs::set_permissions(&release_binary, perms).await;
    }

    // Symlink the cached source tree into the release dir as
    // `src/`. The systemd unit's WorkingDirectory points at
    // `<root>/current/src/server/daemon` so kanidmd's relative
    // `../core/static/...` lookup resolves correctly regardless of
    // which release is active.
    let src_link = new_release.dir.join("src");
    let src_link_target = std::path::Path::new("../..")
        .join("src")
        .join(format!("kanidm-{version}"));
    let target_clone = src_link_target.clone();
    let link_clone = src_link.clone();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(&target_clone, &link_clone))
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(e.to_string())))??;

    // Pre-flight: confirm the freshly-built binary at least
    // loads its dynamic libraries before we swap it in. Catches
    // glibc / sqlite / linker problems BEFORE the running daemon
    // is disrupted.
    //
    // We use `--help` rather than `--version` because kanidmd's
    // top-level clap parser requires a subcommand and rejects
    // bare `--version` as "unexpected argument". `--help` is
    // intercepted by clap before subcommand validation and
    // exits 0 universally; it's the canonical "does this binary
    // run at all" probe.
    progress.set_message("Pre-flighting the new kanidmd binary (--help probe)");
    release::preflight_probe(&new_release, "kanidmd", &["--help"])
        .await
        .map_err(release_error_to_service)?;

    let manifest = ReleaseManifest {
        version: version.to_string(),
        built_at: chrono::Utc::now(),
        binary_sha256: None,
    };
    release::write_manifest(&new_release, &manifest)
        .await
        .map_err(release_error_to_service)?;

    // Stop the running service (if any) BEFORE the symlink swap so
    // the post-install `enable --now` actually launches the new
    // binary. Best-effort: missing-unit failures are harmless.
    let _ = super::systemctl::stop(&opts.unit_name).await;

    progress.set_message(format!(
        "Atomic-swap of <root>/current to the new release {}",
        new_release.id
    ));
    release::make_current(&new_release)
        .await
        .map_err(release_error_to_service)?;

    // Retention: prune all but the most recent N releases. The
    // active release is preserved even when it's older.
    let pruned = release::prune_releases(&opts.root_dir, KANIDM_RELEASE_RETENTION)
        .await
        .map_err(release_error_to_service)?;
    if !pruned.is_empty() {
        info!(
            count = pruned.len(),
            retained = KANIDM_RELEASE_RETENTION,
            "pruned stale kanidm release directories"
        );
    }

    let current_dir = opts.root_dir.join("current");
    let final_binary = current_dir.join("kanidmd");

    // 3. Self-signed TLS cert (kanidm requires TLS even on loopback).
    progress.set_phase(InstallPhase::Initdb);
    progress.set_message("Generating self-signed TLS cert (pure Rust via rcgen)");
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
    //    We can't call `service::install_service` directly because
    //    it expects a Bundle to fetch -- the cargo path replaces
    //    that phase. Reimplement just the systemd registration
    //    here.
    //
    // ExecStart and WorkingDirectory both route through
    // `<root>/current/`, the symlink the release module manages.
    // Future re-installs just swap the symlink atomically; the
    // unit file doesn't need rewriting unless the unit_name or
    // root_dir changes. WorkingDirectory=<root>/current/src/server/daemon
    // makes kanidmd's `../core/static/...` lookup resolve through
    // the release dir's `src` symlink into the cached source
    // tree. ProtectSystem=strict only restricts writes, so the
    // binary can read the static assets without listing the
    // resolved path in ReadWritePaths.
    let working_dir = current_dir.join("src").join("server").join("daemon");
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit_body = systemd_unit(
        &current_dir,
        &server_toml_path,
        &opts.root_dir,
        &working_dir,
    );
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
    // wait_for_port returns NotReady on timeout; enrich it with
    // the systemd journal so the operator sees the actual reason
    // kanidmd crashed (config-schema mismatch, missing native lib,
    // port in use, ...) on the install-result page instead of the
    // generic "did not become ready" string.
    if let Err(e) = wait_for_port("127.0.0.1", opts.port, std::time::Duration::from_secs(60)).await
    {
        if matches!(e, ServiceError::NotReady { .. }) {
            let tail = super::systemctl::journal_tail(&opts.unit_name, 60).await;
            if tail.is_empty() {
                return Err(e);
            }
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "kanidmd did not bind 127.0.0.1:{} within 60s. Journal tail (most recent {} lines from `journalctl -u {} --no-pager`):\n\n{tail}",
                opts.port,
                tail.lines().count(),
                opts.unit_name,
            ))));
        }
        return Err(e);
    }

    // 6. PATH shim for the kanidmd binary (no CLI tools shipped via
    //    this install path; operators install `kanidm_tools`
    //    separately if they want the client). The shim points at
    //    `<root>/current/kanidmd` so it follows future release
    //    swaps automatically -- no need to re-register on upgrade.
    progress.set_phase(InstallPhase::RegisteringPath);
    let cli_symlink = match super::path::register("kanidmd", &final_binary).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "registering kanidmd symlink failed; non-fatal");
            None
        }
    };

    Ok(InstalledService {
        bin_dir: current_dir,
        unit_path,
        port: opts.port,
        cli_symlink,
    })
}

/// Convert a `release::ReleaseError` into the driver's
/// `ServiceError::Io`. The release module has its own error type
/// for separation of concerns; converting at the boundary keeps
/// the driver's outward error surface unchanged.
fn release_error_to_service(e: release::ReleaseError) -> ServiceError {
    ServiceError::Io(std::io::Error::other(e.to_string()))
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

/// Download + extract the upstream Kanidm source tarball under
/// `<src_root>/kanidm-<version>/`. GitHub's archive URL for a tag
/// `v<X>` extracts to a directory named `kanidm-<X>` (the
/// repository name plus the tag without the leading `v`). Cached:
/// if the directory already has a `Cargo.toml`, the extraction is
/// reused.
async fn ensure_kanidm_source_extracted(
    src_root: &Path,
    version: &str,
) -> Result<PathBuf, ServiceError> {
    let extracted = src_root.join(format!("kanidm-{version}"));
    if fs::try_exists(extracted.join("Cargo.toml"))
        .await
        .unwrap_or(false)
    {
        info!(
            extracted = %extracted.display(),
            "kanidm source already extracted; skipping download"
        );
        return Ok(extracted);
    }

    let url = format!("https://github.com/kanidm/kanidm/archive/refs/tags/v{version}.tar.gz");
    let tarball = src_root.join(format!("kanidm-{version}.tar.gz"));
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("GET {url}: {e}"))))?;
    if !resp.status().is_success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "GET {url} returned HTTP {}; verify the tag exists at https://github.com/kanidm/kanidm/tags",
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

    // Shell out to `tar` rather than carry yet another archive
    // dep. `tar` is in coreutils everywhere we run.
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
    let _ = fs::remove_file(&tarball).await;

    if !fs::try_exists(extracted.join("Cargo.toml"))
        .await
        .unwrap_or(false)
    {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "extracted {} but no Cargo.toml inside -- GitHub's archive layout may have changed; expected kanidm-<version>/Cargo.toml",
            extracted.display()
        ))));
    }
    Ok(extracted)
}

/// Run `cargo build --release --bin kanidmd` against the unpacked
/// source tree.
///
/// `--bin kanidmd` selects the binary by name, sidestepping the
/// workspace package-name churn that broke `cargo install kanidmd`
/// on recent tags (the binary lives in a package whose own name
/// has changed across releases; the binary name has been stable).
async fn cargo_build_kanidmd(cargo_bin: &Path, src_dir: &Path) -> Result<(), ServiceError> {
    let out = Command::new(cargo_bin)
        .arg("build")
        .arg("--release")
        .arg("--locked")
        .arg("--bin")
        .arg("kanidmd")
        .arg("--manifest-path")
        .arg(src_dir.join("Cargo.toml"))
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "cargo build --release --bin kanidmd failed (cargo={}, src={}). Common causes: (1) the host Rust toolchain is older than what kanidm needs at this tag (the prerequisites module installs a recent stable; if a host distro cargo is shadowing it on $PATH, move it aside); (2) network egress to crates.io blocked while resolving deps; (3) a missing native lib that kanidm links against (sqlite3-dev on some minimal images). Full stderr below:\n{}",
            cargo_bin.display(),
            src_dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ))));
    }
    Ok(())
}

/// Generate a self-signed TLS cert + private key for the local kanidm
/// service. Pure Rust via `rcgen` -- no shell-out to `openssl`, so a
/// virgin Linux host without the openssl CLI installed can still run
/// the kanidm install path. The cert has CN=localhost and a SAN list
/// of `["localhost", "127.0.0.1"]` so kanidm (and clients) accept it
/// for the loopback bind that v0.0.x ships.
///
/// rcgen picks sane defaults (ECDSA P-256 / SHA-256, validity ending
/// roughly 2 years from now) -- the previous openssl shell-out
/// produced an RSA 2048 / 365-day cert, but the algorithm doesn't
/// matter for a loopback-only self-signed cert and the longer expiry
/// is operator-friendly.
async fn generate_self_signed(cert: &Path, key: &Path) -> Result<(), ServiceError> {
    let cert_path = cert.to_path_buf();
    let key_path = key.to_path_buf();
    let (cert_pem, key_pem) =
        tokio::task::spawn_blocking(move || -> Result<(String, String), rcgen::Error> {
            let sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
            let key_pair = rcgen::KeyPair::generate()?;
            let mut params = rcgen::CertificateParams::new(sans)?;
            params.distinguished_name = {
                let mut dn = rcgen::DistinguishedName::new();
                dn.push(rcgen::DnType::CommonName, "localhost");
                dn
            };
            let cert = params.self_signed(&key_pair)?;
            Ok((cert.pem(), key_pair.serialize_pem()))
        })
        .await
        .map_err(|e| {
            ServiceError::Io(std::io::Error::other(format!(
                "rcgen task join failed: {e}"
            )))
        })?
        .map_err(|e| {
            ServiceError::Io(std::io::Error::other(format!(
                "rcgen self-signed cert generation failed: {e}"
            )))
        })?;

    fs::write(&cert_path, cert_pem).await?;
    fs::write(&key_path, key_pem).await?;
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

fn systemd_unit(bin_dir: &Path, config_path: &Path, root_dir: &Path, working_dir: &Path) -> String {
    // RuntimeDirectory=kanidmd: matches the postgres / xtable
    // forward-compat. Kanidm doesn't currently write to /run/, but
    // adding the directive now is free and prevents a v0.1
    // regression if a future kanidm version starts dropping socket
    // / lock files there.
    //
    // WorkingDirectory=<src>/server/daemon: kanidm resolves its
    // web UI static assets via the relative path
    // `../core/static/...`. Without this directive the assets are
    // looked up relative to systemd's default CWD of `/` and the
    // daemon dies at startup with "Can't find external/...".
    format!(
        "[Unit]\n\
         Description=Computeza-managed Kanidm\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         RuntimeDirectory=kanidmd\n\
         RuntimeDirectoryMode=0755\n\
         WorkingDirectory={cwd}\n\
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
        cwd = working_dir.display(),
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
