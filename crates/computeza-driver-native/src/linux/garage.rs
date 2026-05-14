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

use super::release::{self, ReleaseManifest};
use super::service::{InstalledService, ServiceError, Uninstalled};

/// How many old release directories to retain after a successful
/// install. Matches the kanidm driver; rationale documented in
/// `linux::release`.
const GARAGE_RELEASE_RETENTION: usize = 3;

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

    // 4. Stage the freshly-built binary into a release directory
    //    and atomic-swap `<root>/current` to point at it. systemd
    //    ExecStart references `<root>/current/garage`, so the
    //    symlink swap is what actually upgrades the running
    //    daemon -- never an in-place overwrite of the executing
    //    binary.
    let built = src_dir.join("target").join("release").join("garage");
    if !fs::try_exists(&built).await.unwrap_or(false) {
        return Err(ServiceError::BinaryMissing {
            binary: "garage".into(),
            bin_dir: src_dir.join("target").join("release"),
        });
    }
    progress.set_message(format!(
        "Staging garage v{version} into a fresh release directory"
    ));
    let new_release = release::new_release(&opts.root_dir, version)
        .await
        .map_err(release_error_to_service)?;
    let release_binary = new_release.dir.join("garage");
    fs::copy(&built, &release_binary).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = tokio::fs::set_permissions(&release_binary, perms).await;
    }

    // Pre-flight probe: confirm the binary at least executes
    // `--version` before we swap it in. Catches glibc / linker
    // problems BEFORE disrupting the running release.
    progress.set_message("Pre-flighting the new garage binary (--version probe)");
    release::preflight_probe(&new_release, "garage", &["--version"])
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
    // binary.
    let _ = super::systemctl::stop(&opts.unit_name).await;

    progress.set_message(format!(
        "Atomic-swap of <root>/current to the new release {}",
        new_release.id
    ));
    release::make_current(&new_release)
        .await
        .map_err(release_error_to_service)?;

    let pruned = release::prune_releases(&opts.root_dir, GARAGE_RELEASE_RETENTION)
        .await
        .map_err(release_error_to_service)?;
    if !pruned.is_empty() {
        info!(
            count = pruned.len(),
            retained = GARAGE_RELEASE_RETENTION,
            "pruned stale garage release directories"
        );
    }

    let current_dir = opts.root_dir.join("current");
    let final_binary = current_dir.join("garage");

    // 5. Config + systemd unit.
    //
    // garage.toml is a starting-point template that operators
    // edit -- the rpc_secret, admin_token, and metrics_token all
    // ship as `change-me` placeholders and should be rotated by
    // the operator before exposing the daemon. Preserve those
    // edits across re-installs by writing only when the file
    // doesn't already exist.
    let s3_port = opts.port;
    let rpc_port = s3_port + 1;
    let web_port = s3_port + 2;
    let admin_port = s3_port + 3;
    let config_path = opts.root_dir.join("garage.toml");
    if !fs::try_exists(&config_path).await.unwrap_or(false) {
        let config = garage_toml(&opts.root_dir, s3_port, rpc_port, web_port, admin_port);
        let mut f = fs::File::create(&config_path).await?;
        f.write_all(config.as_bytes()).await?;
        f.sync_all().await?;
        info!(config = %config_path.display(), "wrote garage.toml");
    } else {
        info!(
            config = %config_path.display(),
            "garage.toml already exists; re-install preserves operator edits (delete the file to regenerate)"
        );
    }

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
        bin_dir: current_dir,
        unit_path,
        port: opts.port,
        cli_symlink,
    })
}

// ============================================================
// Post-install bootstrap (per v0.1 design doc §3.2 + §3.3)
// ============================================================

/// Name of the Garage access key we mint exclusively for
/// Lakekeeper's use. Hard-coded -- only one Lakekeeper warehouse
/// uses this key in v0.1; multi-warehouse keying is a v0.2
/// concern.
const BOOTSTRAP_KEY_NAME: &str = "lakekeeper";

/// Garage bucket Lakekeeper writes Iceberg metadata + data into.
/// Hard-coded for the same reason as BOOTSTRAP_KEY_NAME.
const BOOTSTRAP_BUCKET_NAME: &str = "lakekeeper-default";

/// Zone label used when assigning the local Garage node a role.
/// "dc1" is arbitrary -- single-node deployments only have one
/// zone. Multi-node + multi-zone layouts are a v0.2 driver task.
const BOOTSTRAP_ZONE: &str = "dc1";

/// Capacity assigned to the local Garage node at layout-apply
/// time. The right long-term answer is "statvfs(<root>/data) * 0.5
/// rounded down" (per AGENTS.md). For v0.1 we hard-code a value
/// that works on every realistic dev box; operators with smaller
/// disks can re-run layout assign by hand to override.
const BOOTSTRAP_CAPACITY: &str = "10G";

/// Idempotent post-install bootstrap. Run after `install_service`
/// returns + the Garage admin port is accepting connections.
/// Performs three steps in order:
///
///   1. Cluster layout: assign the local node to zone `dc1` with
///      10G capacity and apply the layout. Skipped if the layout
///      is already active (idempotent re-runs).
///   2. Access key: mint a `lakekeeper`-named key. Skipped if the
///      key already exists (which means a previous install already
///      bootstrapped it -- the secret is in the vault, and Garage
///      won't reveal it again).
///   3. Bucket: create `lakekeeper-default` and grant the
///      `lakekeeper` key RWO permissions. Both idempotent on the
///      Garage side.
///
/// Returns one `BootstrapArtifact` per piece of state the
/// orchestrator should persist into the vault + credentials.json.
/// Always returns the bucket-name artifact. Returns the key-id +
/// secret artifacts ONLY on first run (when the key was actually
/// created); on re-runs where the key already exists, returns
/// nothing for those two (the vault is the source of truth -- the
/// orchestrator preserves the existing entries).
pub async fn post_install_bootstrap(
    root_dir: &Path,
) -> Result<Vec<super::BootstrapArtifact>, super::BootstrapError> {
    use secrecy::SecretString;

    let bin = PathBuf::from("/usr/local/bin/computeza-garage");
    let conf = root_dir.join("garage.toml");

    // ---- Step 1: layout ------------------------------------------------
    let status_out = run_garage_cli(&bin, &conf, &["status"]).await?;
    let layout_already_applied = !status_out.contains("NO ROLE ASSIGNED");
    if !layout_already_applied {
        let node_id = parse_local_node_id(&status_out)?;
        info!(node_id = %node_id, "garage bootstrap: assigning layout");
        run_garage_cli(
            &bin,
            &conf,
            &[
                "layout",
                "assign",
                &node_id,
                "-z",
                BOOTSTRAP_ZONE,
                "-c",
                BOOTSTRAP_CAPACITY,
            ],
        )
        .await?;
        // Layout version starts at 0 (un-applied); first apply
        // commits it as version 1. `gg layout apply --version 1`
        // is the explicit-version invocation Garage requires to
        // confirm the operator knows what they're committing.
        run_garage_cli(&bin, &conf, &["layout", "apply", "--version", "1"]).await?;
        info!("garage bootstrap: layout applied (zone={BOOTSTRAP_ZONE}, capacity={BOOTSTRAP_CAPACITY})");
    } else {
        info!("garage bootstrap: layout already applied; skipping");
    }

    // ---- Step 2: access key --------------------------------------------
    let key_info = run_garage_cli(&bin, &conf, &["key", "info", BOOTSTRAP_KEY_NAME])
        .await
        .ok();
    let mut artifacts: Vec<super::BootstrapArtifact> = Vec::new();
    let key_id: String = if let Some(info) = key_info.as_deref() {
        // Key already exists; re-use the existing Key ID. The secret
        // cannot be recovered (Garage redacts on `key info`) so we
        // don't emit a secret artifact -- the orchestrator preserves
        // the vault entry from the original creation.
        let id = parse_key_id(info)?;
        info!(key_id = %id, "garage bootstrap: lakekeeper key already exists; skipping create");
        id
    } else {
        // Create the key; capture both ID + Secret from the create
        // output (this is the ONLY moment Garage reveals the
        // secret). Both go into artifacts so the orchestrator can
        // persist them.
        let create_out =
            run_garage_cli(&bin, &conf, &["key", "create", BOOTSTRAP_KEY_NAME]).await?;
        let id = parse_key_id(&create_out)?;
        let secret = parse_key_secret(&create_out)?;
        info!(key_id = %id, "garage bootstrap: lakekeeper key created");
        artifacts.push(super::BootstrapArtifact {
            vault_key: "garage/lakekeeper-key-id".into(),
            value: SecretString::from(id.clone()),
            label: "Garage Access Key ID (Lakekeeper-scoped)".into(),
            display_inline: true,
        });
        artifacts.push(super::BootstrapArtifact {
            vault_key: "garage/lakekeeper-secret".into(),
            value: SecretString::from(secret),
            label: "Garage Secret Access Key (Lakekeeper-scoped)".into(),
            display_inline: true,
        });
        id
    };

    // ---- Step 3: bucket + allow ----------------------------------------
    // `bucket create` is idempotent enough -- it errors with a
    // specific message if the bucket exists, which we treat as
    // success.
    match run_garage_cli(&bin, &conf, &["bucket", "create", BOOTSTRAP_BUCKET_NAME]).await {
        Ok(_) => info!("garage bootstrap: lakekeeper-default bucket created"),
        Err(super::BootstrapError::CliFailed { stderr, .. })
            if stderr.contains("already exists") || stderr.contains("Bucket already exists") =>
        {
            info!("garage bootstrap: lakekeeper-default bucket already exists; skipping");
        }
        Err(e) => return Err(e),
    }
    // `bucket allow` is idempotent on the Garage side (re-running
    // just re-asserts the grant).
    run_garage_cli(
        &bin,
        &conf,
        &[
            "bucket",
            "allow",
            BOOTSTRAP_BUCKET_NAME,
            "--read",
            "--write",
            "--owner",
            "--key",
            BOOTSTRAP_KEY_NAME,
        ],
    )
    .await?;
    info!("garage bootstrap: lakekeeper key granted RWO on lakekeeper-default");

    // Always emit the bucket-name artifact (non-secret metadata
    // Lakekeeper's bootstrap step needs to read from the vault).
    artifacts.push(super::BootstrapArtifact {
        vault_key: "garage/lakekeeper-bucket".into(),
        value: SecretString::from(BOOTSTRAP_BUCKET_NAME.to_string()),
        label: "Garage bucket name (Lakekeeper-scoped)".into(),
        display_inline: false,
    });

    let _ = key_id; // silence "unused" warning when key already existed
    Ok(artifacts)
}

/// Shell out to the `computeza-garage` CLI with `-c <conf>` and the
/// caller's argv suffix. Returns stdout as a UTF-8 string on
/// success, or a `BootstrapError::CliFailed` carrying stderr on
/// non-zero exit.
async fn run_garage_cli(
    bin: &Path,
    conf: &Path,
    args: &[&str],
) -> Result<String, super::BootstrapError> {
    let mut cmd = Command::new(bin);
    cmd.arg("-c").arg(conf);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(super::BootstrapError::CliFailed {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the local node ID from `gg status` output. The healthy-
/// nodes table has columns `ID | Hostname | Address | ...`; we
/// pick the first 16-hex token on a line that looks like a node
/// row (16-char lowercase hex prefix, then whitespace).
fn parse_local_node_id(status_out: &str) -> Result<String, super::BootstrapError> {
    // Match a 16-char hex string at the start of a line (allowing
    // leading whitespace), optionally followed by more text. Single-
    // node clusters have exactly one such line.
    let re = regex_lite_first_match(status_out, |line| {
        let trimmed = line.trim_start();
        if trimmed.len() < 16 {
            return None;
        }
        let prefix = &trimmed[..16];
        if prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(prefix.to_string())
        } else {
            None
        }
    });
    re.ok_or_else(|| super::BootstrapError::ParseFailed {
        what: "garage node ID (16-char hex prefix)".into(),
        output: status_out.to_string(),
    })
}

/// Parse the `Key ID:` line out of `gg key info` / `gg key create`
/// output.
fn parse_key_id(out: &str) -> Result<String, super::BootstrapError> {
    parse_field(out, "Key ID:").ok_or_else(|| super::BootstrapError::ParseFailed {
        what: "garage `Key ID:` field".into(),
        output: out.to_string(),
    })
}

/// Parse the `Secret key:` line out of `gg key create` output.
/// Returns an error if the value is `(redacted)` -- that means we
/// called this on `gg key info` output by mistake (Garage redacts
/// the secret on subsequent reads).
fn parse_key_secret(out: &str) -> Result<String, super::BootstrapError> {
    let v = parse_field(out, "Secret key:").ok_or_else(|| super::BootstrapError::ParseFailed {
        what: "garage `Secret key:` field".into(),
        output: out.to_string(),
    })?;
    if v == "(redacted)" {
        return Err(super::BootstrapError::StateMismatch(
            "garage `Secret key:` was redacted; the key already existed and the secret cannot be recovered. \
             Rotate via `gg key delete lakekeeper && gg key create lakekeeper` (re-grant bucket access after) \
             and re-run install.".into(),
        ));
    }
    Ok(v)
}

/// Generic "Field: value" line extractor. Returns the trimmed
/// value following the first occurrence of `prefix` at the start
/// of a line. Used for Garage CLI output which uses fixed
/// "Label:                value" formatting.
fn parse_field(out: &str, prefix: &str) -> Option<String> {
    for line in out.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Tiny line-scanning helper used by parse_local_node_id -- avoids
/// pulling in the `regex` crate dependency for the one use case.
/// Returns the first line that matches the closure.
fn regex_lite_first_match<T>(text: &str, f: impl Fn(&str) -> Option<T>) -> Option<T> {
    text.lines().find_map(f)
}

/// Convert a `release::ReleaseError` into the driver's
/// `ServiceError::Io`. Matches the kanidm driver's boundary
/// convention (the release module has its own error type for
/// separation of concerns; we squash to the driver's outward
/// error shape here).
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
