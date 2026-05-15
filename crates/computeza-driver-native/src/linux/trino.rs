//! Trino distributed SQL engine. Linux install path.
//!
//! Trino is the canonical Iceberg-REST query engine -- Lakekeeper's
//! own docs use it as the reference integration. Computeza ships it
//! as the SQL surface for Studio after the multi-day Databend
//! Iceberg-REST debugging cycle proved Databend's support is
//! community-grade. Sail continues to handle Python/Spark Connect
//! queries; both engines point at the same Lakekeeper catalog.
//!
//! # Version pin: Trino 442
//!
//! Trino 481 (current at time of writing) requires Java 25; bundling
//! a fresh Temurin 25 distribution would mean a second JRE on disk
//! beside the Temurin 21 xtable already bundles via
//! `prerequisites::ensure_bundled_temurin_jre`. Pinning Trino 442 --
//! the last release on Java 21 LTS -- lets us reuse the same JRE
//! bundle and keeps the install footprint tighter. A follow-up pin
//! bump to a Java 25-aware Trino can happen in a focused commit when
//! Temurin 25 has stabilised.
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

/// Trino bundle pins. 442 is the final release on Java 21 LTS;
/// reusing the same JRE bundle xtable already installs keeps the
/// disk footprint tight. To bump past 442, the bundled JRE in
/// `ensure_bundled_temurin_jre` must move to Java 25 first (Trino
/// 481+ refuses to start under older JVMs).
///
/// The Trino project publishes release artifacts to Maven Central
/// (not GitHub releases -- the github.com/trinodb/trino/releases
/// page only links source tarballs, which would force a full
/// Maven build at install time). The canonical download URL is
/// `https://repo1.maven.org/maven2/io/trino/trino-server/<v>/trino-server-<v>.tar.gz`
/// per Trino's own install docs.
const TRINO_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "442",
        url: "https://repo1.maven.org/maven2/io/trino/trino-server/442/trino-server-442.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "bin",
    },
    Bundle {
        version: "441",
        url: "https://repo1.maven.org/maven2/io/trino/trino-server/441/trino-server-441.tar.gz",
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
    progress.set_message("Ensuring bundled Temurin JRE 21".to_string());
    fs::create_dir_all(&opts.root_dir).await?;
    let jre_home = prerequisites::ensure_bundled_temurin_jre(&opts.root_dir, progress)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("bundle JRE: {e}"))))?;

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
         # catalog-management=DYNAMIC enables runtime CREATE CATALOG SQL.\n\
         # Computeza writes etc/catalog/<warehouse>.properties files at\n\
         # bootstrap time; this flag lets operators also use SQL DDL if\n\
         # they prefer. Both surfaces stay in sync because the SQL form\n\
         # writes the same file under the hood.\n\
         catalog-management=DYNAMIC\n",
        port = opts.port,
        discovery_uri = discovery_uri,
    );
    // jvm.config is consumed verbatim by Trino's launcher script.
    // -Xmx2G keeps the heap modest for dev installs; production
    // workloads should bump this. Other args are Trino's
    // recommended starting set from the deployment guide.
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
                      -Dfile.encoding=UTF-8\n\
                      # Defensive opens for Trino's reflection-heavy code paths.\n\
                      -XX:+UnlockDiagnosticVMOptions\n\
                      -XX:GCLockerRetryAllocationCount=32\n"
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

    // Step 3: systemd unit. Trino's `launcher run` is the foreground
    // invocation systemd-managed services should use; `launcher
    // start` daemonises which conflicts with systemd's process
    // tracking. PATH/JAVA_HOME point at the bundled JRE.
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Writing systemd unit {}", opts.unit_name));
    let launcher_path = bin_dir.join("launcher");
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
         ExecStart={launcher} run\n\
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
        launcher = launcher_path.display(),
        install = install_dir.display(),
    );
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    fs::write(&unit_path, unit_body).await?;
    super::systemctl::daemon_reload().await?;

    // Step 4: enable + start + readiness probe.
    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    let _ = super::systemctl::stop(&opts.unit_name).await;
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
        &launcher_path,
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
