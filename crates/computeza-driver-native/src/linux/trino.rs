//! Trino distributed SQL engine. Linux install path.
//!
//! Trino is the canonical Iceberg-REST query engine -- Lakekeeper's
//! own docs use it as the reference integration. Computeza ships it
//! as the SQL surface for Studio after the multi-day Databend
//! Iceberg-REST debugging cycle proved Databend's support is
//! community-grade. Sail continues to handle Python/Spark Connect
//! queries; both engines point at the same Lakekeeper catalog.
//!
//! # Version pin: Trino 470
//!
//! Trino 470 (verified available on Maven Central) ships the
//! `catalog-management=DYNAMIC` flag (introduced in Trino 459)
//! which lets the Studio bootstrap path use `CREATE CATALOG ...`
//! SQL DDL to register Iceberg-REST catalogs at runtime -- no
//! coordinator restart needed. Trino 470 requires Java 23+; we
//! bundle a dedicated Temurin 25 LTS JRE for it
//! (`prerequisites::ensure_bundled_temurin_jre_25`) so xtable can
//! keep its Java 21 bundle unchanged. The disk overhead of carrying
//! both JREs (~100MB) is the price of keeping Spark-based xtable on
//! a runtime it's tested against.
//!
//! # Layout on disk
//!
//! ```text
//! /var/lib/computeza/trino/
//! ├── binaries/<version>/        ← unpacked trino-server-<v>/
//! │   ├── bin/launcher
//! │   ├── lib/...
//! │   ├── plugin/iceberg/...     ← Iceberg connector with REST support
//! │   └── etc/                   ← our config files (Trino reads from here)
//! │       ├── node.properties
//! │       ├── config.properties
//! │       ├── jvm.config
//! │       ├── log.properties
//! │       └── catalog/
//! │           └── <warehouse>.properties  ← written by bootstrap, hot-reloaded
//! ├── jre/                       ← bundled Temurin 21 (shared style with xtable)
//! └── data/                      ← node.data-dir target
//! ```

use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::{
    fetch::{ArchiveKind, Bundle},
    prerequisites,
    progress::{InstallPhase, ProgressHandle},
};

use super::service::{self, InstalledService, ServiceError, Uninstalled};

pub const UNIT_NAME: &str = "computeza-trino.service";

/// Trino's HTTP server port. Default in Trino's own examples is 8080,
/// which collides with our OpenFGA pin; 8088 is unused in the
/// Computeza port plan and is close enough to the default that
/// operators searching docs for "trino 8080" find their way.
pub const DEFAULT_PORT: u16 = 8088;

/// Trino bundle pins. 470 is the first version available on Maven
/// Central with both DYNAMIC catalog management (459+) and a stable
/// release cadence; 459+ also requires Java 22+, so the install
/// path pulls Temurin 25 LTS instead of the Java 21 bundle xtable
/// uses.
///
/// The Trino project publishes release artifacts to Maven Central
/// (not GitHub releases -- the github.com/trinodb/trino/releases
/// page only links source tarballs, which would force a full
/// Maven build at install time). The canonical download URL is
/// `https://repo1.maven.org/maven2/io/trino/trino-server/<v>/trino-server-<v>.tar.gz`
/// per Trino's own install docs.
const TRINO_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "470",
        url: "https://repo1.maven.org/maven2/io/trino/trino-server/470/trino-server-470.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "bin",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    TRINO_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/trino"),
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
    let bundle = pick_bundle(opts.version.as_deref()).clone();

    // Step 1: bundle Temurin JRE 21 (shared with xtable's pin). The
    // helper is idempotent -- if the JRE is already extracted under
    // <root>/jre/ from a previous install, this is fast.
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Ensuring bundled Temurin JRE 25".to_string());
    fs::create_dir_all(&opts.root_dir).await?;
    // Trino 470 needs Java 22+. We bundle Java 25 LTS (separate
    // from xtable's Java 21 bundle so Spark's runtime support
    // matrix isn't disturbed). ensure_bundled_temurin_jre_25
    // returns the path to the `java` binary
    // (<jre-root>/bin/java), not the JRE root itself. Walk two
    // parents up to get JAVA_HOME (= <jre-root>) for systemd's
    // JAVA_HOME/PATH environment.
    let java_bin = prerequisites::ensure_bundled_temurin_jre_25(&opts.root_dir, progress)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("bundle JRE: {e}"))))?;
    let jre_home = java_bin
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| {
            ServiceError::Io(std::io::Error::other(format!(
                "could not derive JAVA_HOME from java binary path {}",
                java_bin.display()
            )))
        })?;

    // Step 2: write Trino's etc/ config files BEFORE invoking
    // install_service so the unpacked tarball's etc/ subdir is
    // populated by the time launcher first reads it. The bundle's
    // etc/ doesn't ship with the tarball -- it's our responsibility.
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message("Writing Trino etc/ config files".to_string());

    // Discovery URI is the coordinator's own HTTP endpoint; for the
    // single-node deploy this is the same host:port the operator
    // hits.
    let discovery_uri = format!("http://127.0.0.1:{port}", port = opts.port);

    let node_properties = format!(
        "node.environment=production\n\
         node.id=computeza-trino-node-1\n\
         node.data-dir={root}/data\n",
        root = opts.root_dir.display(),
    );
    let config_properties = format!(
        "coordinator=true\n\
         node-scheduler.include-coordinator=true\n\
         http-server.http.port={port}\n\
         discovery.uri={discovery_uri}\n\
         # query.max-memory: per-query distributed memory cap (sum across workers).\n\
         # 2GB is enough for v0.0.x dev installs; production should bump this.\n\
         query.max-memory=2GB\n\
         query.max-memory-per-node=1GB\n\
         # catalog-management=DYNAMIC enables runtime CREATE CATALOG SQL\n\
         # (introduced in Trino 459). Computeza writes etc/catalog/<warehouse>.properties\n\
         # files at bootstrap time AND submits CREATE CATALOG via /v1/statement\n\
         # so the new catalog is usable immediately, no coordinator restart.\n\
         catalog.management=dynamic\n\
         # catalog.store=file makes the in-memory catalog state persist on\n\
         # disk so DYNAMIC-created catalogs survive a coordinator restart\n\
         # in addition to the etc/catalog/ files.\n\
         catalog.store=file\n",
        port = opts.port,
        discovery_uri = discovery_uri,
    );
    // jvm.config is consumed verbatim by Trino's launcher script.
    // -Xmx2G keeps the heap modest for dev installs; production
    // workloads should bump this. Args are Trino's recommended
    // starting set from the deployment guide, with two flags
    // dropped that Java 25 no longer accepts:
    //   * -XX:+UnlockDiagnosticVMOptions
    //   * -XX:GCLockerRetryAllocationCount=32
    // (The latter was a Java 22 band-aid for a specific GC issue;
    // Java 25 removed both the symptom and the flag.)
    let jvm_config = "-server\n\
                      -Xmx2G\n\
                      -XX:InitialRAMPercentage=80\n\
                      -XX:MaxRAMPercentage=80\n\
                      -XX:G1HeapRegionSize=32M\n\
                      -XX:+ExplicitGCInvokesConcurrent\n\
                      -XX:+ExitOnOutOfMemoryError\n\
                      -XX:+HeapDumpOnOutOfMemoryError\n\
                      -XX:-OmitStackTraceInFastThrow\n\
                      -XX:ReservedCodeCacheSize=512M\n\
                      -XX:PerMethodRecompilationCutoff=10000\n\
                      -XX:PerBytecodeRecompilationCutoff=10000\n\
                      -Djdk.attach.allowAttachSelf=true\n\
                      -Djdk.nio.maxCachedBufferSize=2000000\n\
                      -Dfile.encoding=UTF-8\n"
        .to_string();
    let log_properties = "io.trino=INFO\n".to_string();

    // We can't write etc/ until the tarball is unpacked. Delegate
    // the install_service step now, then write etc/ into the
    // unpacked dir, THEN start the service.
    //
    // service::install_service handles: download tarball, extract,
    // install systemd unit, enable + start, wait_for_port. We need
    // etc/ in place BEFORE the start step or Trino's launcher
    // exits with "Cannot find etc/node.properties".
    //
    // The clean way: use a custom flow rather than install_service's
    // monolithic pipeline. Below: download + extract first, then
    // write etc/, then unit + start.

    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!("Fetching Trino server {}", bundle.version));
    let cache_root = opts.root_dir.join("binaries");
    fs::create_dir_all(&cache_root).await?;
    // fetch_and_extract returns <cache>/<version>/<bin_subpath>.
    // For Trino the tarball is a wrapper `trino-server-<v>/` dir
    // containing bin/lib/plugin -- the Bundle bin_subpath value of
    // "bin" lands ABOVE the wrapper; the real launcher lives at
    // <cache>/<version>/trino-server-<v>/bin/launcher. Discover the
    // actual install_dir by walking the version dir for a child
    // matching "trino-server-*".
    let _ignored_bin_dir = crate::fetch::fetch_and_extract(&cache_root, &bundle, progress)
        .await
        .map_err(ServiceError::Fetch)?;
    let version_root = cache_root.join(bundle.version);
    let install_dir = find_trino_install_root(&version_root).await.ok_or_else(|| {
        ServiceError::Io(std::io::Error::other(format!(
            "Trino extract: no trino-server-<v>/ directory found under {}",
            version_root.display()
        )))
    })?;
    let bin_dir = install_dir.join("bin");

    let etc_dir = install_dir.join("etc");
    fs::create_dir_all(&etc_dir).await?;
    fs::create_dir_all(etc_dir.join("catalog")).await?;
    write_file(&etc_dir.join("node.properties"), &node_properties).await?;
    write_file(&etc_dir.join("config.properties"), &config_properties).await?;
    write_file(&etc_dir.join("jvm.config"), &jvm_config).await?;
    write_file(&etc_dir.join("log.properties"), &log_properties).await?;
    fs::create_dir_all(opts.root_dir.join("data")).await?;
    info!(
        etc_dir = %etc_dir.display(),
        "trino: wrote node/config/jvm/log properties"
    );

    // Trino bin/ layout has changed across versions:
    //   * Trino <=442: bin/launcher.py is the Python entrypoint
    //     (with bin/launcher as a shell wrapper).
    //   * Trino 470+: bin/launcher is a SHELL script that finds java
    //     and execs the server JAR directly (no Python anywhere).
    // We pick the right entrypoint and decide whether to wrap with
    // python3 based on its actual shebang. For older Python
    // launchers we ALSO patch `#!/usr/bin/env python` ->
    // `#!/usr/bin/env python3` so direct CLI use works on distros
    // without the unversioned `python` binary.
    let launcher_py_path = bin_dir.join("launcher.py");
    let launcher_bare = bin_dir.join("launcher");
    let entry = if fs::try_exists(&launcher_py_path).await.unwrap_or(false) {
        launcher_py_path.clone()
    } else {
        launcher_bare.clone()
    };
    let entry_kind = detect_launcher_kind(&entry).await;
    if matches!(entry_kind, LauncherKind::Python) {
        if let Ok(body) = fs::read_to_string(&entry).await {
            if body.starts_with("#!/usr/bin/env python\n")
                || body.starts_with("#!/usr/bin/env python\r\n")
            {
                let patched = body.replacen(
                    "#!/usr/bin/env python",
                    "#!/usr/bin/env python3",
                    1,
                );
                fs::write(&entry, patched).await?;
                info!(path = %entry.display(), "trino: patched python shebang to python3");
            }
        }
    }
    info!(
        path = %entry.display(),
        kind = ?entry_kind,
        "trino: launcher entry resolved"
    );

    // Step 3: systemd unit. Trino's `launcher run` is the foreground
    // invocation systemd-managed services should use; `launcher
    // start` daemonises which conflicts with systemd's process
    // tracking. PATH/JAVA_HOME point at the bundled JRE.
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Writing systemd unit {}", opts.unit_name));
    // Compose ExecStart based on what kind of launcher we picked:
    //   * Python: prepend an absolute /usr/bin/python3 to bypass the
    //     `#!/usr/bin/env python` shebang on python-less distros.
    //   * Shell:  execute the shell launcher directly via its own
    //     shebang (Trino 470's launcher is shell + handles java
    //     discovery + exec internally).
    let exec_start = match entry_kind {
        LauncherKind::Python => {
            let python3 = detect_python3().await.ok_or_else(|| {
                ServiceError::Io(std::io::Error::other(
                    "python3 not found on PATH (looked in /usr/bin/python3, /usr/local/bin/python3, \
                     $(which python3)). Install python3 from your distro's package manager \
                     (e.g. `sudo apt install python3` on Debian/Ubuntu) and re-run the Trino install.",
                ))
            })?;
            info!(python3 = %python3.display(), "trino: invoking launcher via python3");
            format!("{} {} run", python3.display(), entry.display())
        }
        LauncherKind::Shell | LauncherKind::Unknown => {
            info!(launcher = %entry.display(), "trino: invoking shell launcher directly");
            format!("{} run", entry.display())
        }
    };
    let unit_body = format!(
        "[Unit]\n\
         Description=Computeza-managed Trino SQL engine\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=\"JAVA_HOME={jre_home}\"\n\
         Environment=\"PATH={jre_home}/bin:/usr/bin:/bin\"\n\
         ExecStart={exec_start}\n\
         WorkingDirectory={install}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # Trino's startup is slow (~15-30s); systemd should wait\n\
         # before declaring the service failed.\n\
         TimeoutStartSec=120\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        jre_home = jre_home.display(),
        exec_start = exec_start,
        install = install_dir.display(),
    );
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    fs::write(&unit_path, unit_body).await?;
    super::systemctl::daemon_reload().await?;

    // Step 4: enable + start + readiness probe.
    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    let _ = super::systemctl::stop(&opts.unit_name).await;
    // Clear any restart-loop failure state from a prior bad install
    // (typically: launcher's #!/usr/bin/env python on a system
    // without python). Otherwise systemd remembers the prior
    // 'failed' state and refuses to start until reset-failed.
    let _ = super::systemctl::reset_failed(&opts.unit_name).await;
    super::systemctl::enable_now(&opts.unit_name).await?;

    progress.set_phase(InstallPhase::WaitingForReady);
    progress.set_message(format!("Waiting for Trino HTTP on {}", opts.port));
    if let Err(e) = service::wait_for_port(
        "127.0.0.1",
        opts.port,
        std::time::Duration::from_secs(120),
    )
    .await
    {
        if matches!(e, ServiceError::NotReady { .. }) {
            let tail = super::systemctl::journal_tail(&opts.unit_name, 80).await;
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "trino did not bind 127.0.0.1:{} within 120s. \
                 Trino's startup is slow on first run (JIT compilation + plugin scan). \
                 Journal tail (most recent 80 lines from `journalctl -u {}`):\n\n{tail}",
                opts.port, opts.unit_name
            ))));
        }
        return Err(e);
    }

    // Drop a thin shim into /usr/local/bin so operators can
    // `computeza-trino-cli` from anywhere. The Trino CLI is a
    // separate JAR download; v0.0.x just exposes the launcher.
    let _ = super::path::register(
        "trino",
        &entry,
    )
    .await;

    Ok(InstalledService {
        bin_dir,
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
            root_dir: PathBuf::from("/var/lib/computeza/trino"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    super::service::uninstall_service("trino", &opts.root_dir, &opts.unit_name, Some("trino"))
        .await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/trino");
    if !tokio::fs::try_exists(root.join("data")).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-trino".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => TRINO_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&TRINO_BUNDLES[0]),
        None => &TRINO_BUNDLES[0],
    }
}

async fn write_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut f = fs::File::create(path).await?;
    f.write_all(contents.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

/// What kind of script is `bin/launcher` (or `bin/launcher.py`)?
/// Drives whether the systemd ExecStart prepends `python3` (Python
/// entrypoint, Trino <=442 layout) or runs the file directly via
/// its own shebang (shell launcher, Trino 470+ layout).
#[derive(Debug, Clone, Copy)]
enum LauncherKind {
    Python,
    Shell,
    /// Couldn't read the file or read no shebang. We treat this as
    /// "shell" since that's the safer default with modern Trino;
    /// the error surfaces in journalctl if our guess is wrong.
    Unknown,
}

async fn detect_launcher_kind(path: &Path) -> LauncherKind {
    let Ok(body) = fs::read_to_string(path).await else {
        return LauncherKind::Unknown;
    };
    let first_line = body.lines().next().unwrap_or("");
    if first_line.starts_with("#!/usr/bin/env python")
        || first_line.starts_with("#!/usr/bin/python")
        || first_line.contains("python")
    {
        LauncherKind::Python
    } else if first_line.starts_with("#!/bin/sh")
        || first_line.starts_with("#!/bin/bash")
        || first_line.starts_with("#!/usr/bin/env sh")
        || first_line.starts_with("#!/usr/bin/env bash")
    {
        LauncherKind::Shell
    } else {
        LauncherKind::Unknown
    }
}

/// Resolve an absolute path to a usable `python3` binary for the
/// systemd unit's `ExecStart`. systemd does NOT expand `$PATH` for
/// `ExecStart=`, so a bare `python3` would 127-loop. We check the
/// two standard locations first (cheap stat calls), then fall back
/// to `command -v` for non-standard installs (Homebrew on Linux,
/// asdf, pyenv, etc.). Returns None if no python3 is installed at
/// all -- the caller surfaces an install-time error.
async fn detect_python3() -> Option<PathBuf> {
    for candidate in ["/usr/bin/python3", "/usr/local/bin/python3"] {
        if fs::try_exists(candidate).await.unwrap_or(false) {
            return Some(PathBuf::from(candidate));
        }
    }
    // Last-resort: shell out to `command -v python3` so we pick up
    // ~/.local/bin, /opt/*/bin, pyenv shims, etc. that aren't in
    // the standard locations.
    use tokio::process::Command;
    if let Ok(out) = Command::new("sh")
        .arg("-c")
        .arg("command -v python3")
        .output()
        .await
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() && std::path::Path::new(&path).is_absolute() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

/// Discover the unpacked `trino-server-<v>/` directory inside a
/// version cache root. Trino's tarball is a wrapper-dir-style
/// archive: extracting `trino-server-442.tar.gz` into
/// `/var/lib/computeza/trino/binaries/442/` produces
/// `/var/lib/computeza/trino/binaries/442/trino-server-442/bin/...`.
/// We walk for the first child whose name starts with `trino-server-`
/// rather than hard-coding the version-stamped name (so re-installs
/// with a different pin don't break).
async fn find_trino_install_root(version_root: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(version_root).await.ok()?;
    while let Some(entry) = entries.next_entry().await.ok().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("trino-server-") {
            return Some(entry.path());
        }
    }
    None
}

/// Write or replace a Trino Iceberg-REST catalog properties file.
/// Trino's `catalog-management=DYNAMIC` flag (set in our generated
/// config.properties) lets the operator add/replace catalogs at
/// runtime; without DYNAMIC, Trino picks up new files in
/// etc/catalog/ only on restart.
///
/// Best-effort: returns Err only when the file write itself fails.
/// The caller decides whether to also restart Trino (recommended
/// for first-time registration; CALL system.runtime.refresh_catalogs()
/// is the in-band alternative when DYNAMIC is enabled).
pub async fn write_iceberg_rest_catalog_file(
    root_dir: &Path,
    catalog_name: &str,
    properties: &TrinoIcebergRestConfig,
) -> std::io::Result<PathBuf> {
    // Two-level walk: <root>/binaries/<version>/trino-server-<v>/etc/catalog/.
    // Each version cache root has exactly one trino-server-* child
    // (Bundle pins flush + re-extract on a different version).
    let binaries_root = root_dir.join("binaries");
    let mut version_iter = fs::read_dir(&binaries_root).await?;
    let mut catalog_dir: Option<PathBuf> = None;
    while let Some(version_entry) = version_iter.next_entry().await? {
        if let Some(install_root) = find_trino_install_root(&version_entry.path()).await {
            let p = install_root.join("etc").join("catalog");
            if fs::try_exists(&p).await.unwrap_or(false) {
                catalog_dir = Some(p);
                break;
            }
        }
    }
    let catalog_dir = catalog_dir.ok_or_else(|| {
        std::io::Error::other(
            "Trino install dir not found (no binaries/*/trino-server-*/etc/catalog/). \
             Re-install Trino from /install.",
        )
    })?;

    let file_path = catalog_dir.join(format!("{catalog_name}.properties"));
    let body = format!(
        "connector.name=iceberg\n\
         iceberg.catalog.type=rest\n\
         iceberg.rest-catalog.uri={uri}\n\
         iceberg.rest-catalog.warehouse={warehouse}\n\
         # Native S3 file system properties for the local Garage\n\
         # (or any S3-compatible store). path-style is required\n\
         # for non-AWS endpoints; the region must match Garage's\n\
         # configured s3_region (Garage's default is `garage`).\n\
         fs.s3.enabled=true\n\
         s3.endpoint={s3_endpoint}\n\
         s3.region={s3_region}\n\
         s3.path-style-access=true\n\
         s3.aws-access-key={s3_access_key}\n\
         s3.aws-secret-key={s3_secret_key}\n",
        uri = properties.rest_catalog_uri,
        warehouse = properties.warehouse,
        s3_endpoint = properties.s3_endpoint,
        s3_region = properties.s3_region,
        s3_access_key = properties.s3_access_key,
        s3_secret_key = properties.s3_secret_key,
    );
    fs::write(&file_path, body).await?;
    info!(
        catalog = catalog_name,
        path = %file_path.display(),
        "trino: wrote iceberg-rest catalog properties"
    );
    Ok(file_path)
}

/// Shape of the values needed to write a Trino Iceberg-REST
/// catalog properties file. Mirrors the form fields the Studio
/// bootstrap collects.
#[derive(Clone, Debug)]
pub struct TrinoIcebergRestConfig {
    pub rest_catalog_uri: String,
    pub warehouse: String,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,
}

/// Restart the Trino coordinator and wait for its HTTP endpoint to
/// answer again. Used by the bootstrap path after dropping a new
/// catalog .properties file -- Trino 442 reads catalogs only at
/// startup, so a restart is required for new catalogs to register.
///
/// `unit_name` is typically [`UNIT_NAME`] (`computeza-trino.service`).
/// Waits up to 90 seconds for the port to come back; returns an
/// error containing the journal tail if the coordinator doesn't
/// re-bind in that window.
pub async fn restart_and_wait(unit_name: &str, port: u16) -> Result<(), ServiceError> {
    super::systemctl::run(&["restart", unit_name])
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!(
            "systemctl restart {unit_name}: {e}"
        ))))?;
    // Coordinator restart usually re-binds inside 30s; give it
    // 90s of headroom for cold JIT.
    if let Err(e) = service::wait_for_port(
        "127.0.0.1",
        port,
        std::time::Duration::from_secs(90),
    )
    .await
    {
        if matches!(e, ServiceError::NotReady { .. }) {
            let tail = super::systemctl::journal_tail(unit_name, 40).await;
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "trino did not re-bind 127.0.0.1:{port} within 90s after restart. \
                 Journal tail:\n\n{tail}"
            ))));
        }
        return Err(e);
    }
    Ok(())
}

/// Post-install bootstrap for Trino. v0.0.x runs unauthenticated --
/// the coordinator only checks `X-Trino-User` for query attribution,
/// so there's no password to mint. We still surface connection
/// details (HTTP coordinator URL, JDBC URL, default user) into the
/// credentials.json export so operators have everything they need
/// to point an external client at the cluster without digging
/// through docs.
pub async fn post_install_bootstrap(
    port: u16,
) -> Result<Vec<super::BootstrapArtifact>, super::BootstrapError> {
    use secrecy::SecretString;
    let http_url = format!("http://127.0.0.1:{port}");
    let jdbc_url = format!("jdbc:trino://127.0.0.1:{port}");
    let mut out = Vec::with_capacity(3);
    out.push(super::BootstrapArtifact {
        vault_key: "trino/coordinator-url".into(),
        value: SecretString::from(http_url),
        label: "Trino coordinator HTTP URL".into(),
        display_inline: true,
    });
    out.push(super::BootstrapArtifact {
        vault_key: "trino/jdbc-url".into(),
        value: SecretString::from(jdbc_url),
        label: "Trino JDBC URL".into(),
        display_inline: true,
    });
    out.push(super::BootstrapArtifact {
        vault_key: "trino/default-user".into(),
        value: SecretString::from("computeza".to_string()),
        label: "Trino default user (no password required in v0.0.x)".into(),
        display_inline: true,
    });
    Ok(out)
}
