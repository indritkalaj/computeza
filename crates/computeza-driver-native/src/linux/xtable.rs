//! Apache XTable Iceberg<->Delta<->Hudi metadata sync. Linux install path.
//!
//! # Distribution channel
//!
//! Apache distributes the runner artifact in three forms (per the
//! AGENTS.md "xtable: open infrastructure question" section), and
//! v0.0.x's install path takes the **Maven Central** route:
//!
//! 1. We ensure a sandboxed Temurin JRE 21 lives under
//!    `<root>/jre/` via [`crate::prerequisites::ensure_bundled_temurin_jre`].
//! 2. We require `mvn` on host `$PATH` -- detected-and-surfaced via
//!    [`crate::prerequisites::SYSTEM_COMMANDS`]. (The Apache Maven
//!    runtime is the path we lean on to resolve the ~50 transitive
//!    deps of `org.apache.xtable:xtable-utilities` at install time.)
//! 3. We write a one-off `pom.xml` under `<root>/build/` and shell to
//!    `mvn dependency:copy-dependencies` which lands the classpath
//!    under `<root>/lib/`.
//! 4. The systemd unit runs the bundled `java -cp <root>/lib/*` with
//!    the xtable RunSync main class.
//!
//! v0.1+ replaces the Maven-resolve step with a Computeza-built fat
//! JAR served from a CDN; the wider driver shape doesn't change.

use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::info;

use crate::progress::{InstallPhase, ProgressHandle};

use super::service::{InstalledService, ServiceError, Uninstalled};

pub const SERVICE_NAME: &str = "computeza-xtable";
pub const UNIT_NAME: &str = "computeza-xtable.service";
pub const DEFAULT_PORT: u16 = 8090;

/// Pinned versions of `org.apache.xtable:xtable-utilities`. First
/// entry is the default ("latest"). Versions correspond to Apache
/// XTable release tags.
pub const XTABLE_VERSIONS: &[&str] = &["0.3.0-incubating", "0.2.0-incubating"];

#[must_use]
pub fn available_versions() -> &'static [&'static str] {
    XTABLE_VERSIONS
}

/// Install-options shape mirroring the other component drivers so
/// the unified install can target it with the same per-slug
/// InstallConfig.
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Root directory the install owns. Receives `jre/` (Temurin),
    /// `build/pom.xml` (Maven dispatch), `lib/` (resolved classpath).
    pub root_dir: PathBuf,
    /// TCP port the runner binds. Currently informational --
    /// xtable's RunSync doesn't open a port; the field exists for
    /// future versions and for parity with the InstallConfig shape.
    pub port: u16,
    /// systemd unit name.
    pub unit_name: String,
    /// xtable-utilities version. `None` resolves to
    /// `XTABLE_VERSIONS[0]`.
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/xtable"),
            port: DEFAULT_PORT,
            unit_name: UNIT_NAME.into(),
            version: None,
        }
    }
}

/// Install path:
///
/// 1. Resolve `mvn` on `$PATH` (operator must have Maven). The
///    [`crate::prerequisites::SYSTEM_COMMANDS`] table surfaces the
///    install hint in the wizard banner.
/// 2. Bundle Temurin JRE 21 into `<root>/jre/`.
/// 3. Write a one-off `pom.xml` under `<root>/build/` declaring the
///    `xtable-utilities` dep.
/// 4. Shell to `mvn dependency:copy-dependencies` with
///    `<root>/lib` as the output directory. Maven resolves the deps
///    against Maven Central + downloads each into `<root>/lib/`.
/// 5. Register a systemd unit that runs the bundled JRE against the
///    resulting classpath.
pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    let version = opts.version.as_deref().unwrap_or(XTABLE_VERSIONS[0]);

    // 1. Host prereq: mvn must be on $PATH.
    progress.set_phase(InstallPhase::DetectingBinaries);
    progress.set_message("Verifying Maven (mvn) is on PATH for the xtable dep-resolve step");
    require_on_path("mvn").await?;

    // 2. JRE.
    fs::create_dir_all(&opts.root_dir).await?;
    let java_bin = crate::prerequisites::ensure_bundled_temurin_jre(&opts.root_dir, progress)
        .await
        .map_err(ServiceError::Io)?;

    // 3. pom.xml.
    progress.set_phase(InstallPhase::Extracting);
    progress.set_message(format!(
        "Writing build/pom.xml for xtable-utilities@{version}"
    ));
    let build_dir = opts.root_dir.join("build");
    fs::create_dir_all(&build_dir).await?;
    let pom = pom_xml(version);
    let pom_path = build_dir.join("pom.xml");
    let mut f = fs::File::create(&pom_path).await?;
    f.write_all(pom.as_bytes()).await?;
    f.sync_all().await?;

    // 4. mvn dependency:copy-dependencies.
    let lib_dir = opts.root_dir.join("lib");
    fs::create_dir_all(&lib_dir).await?;
    progress.set_phase(InstallPhase::Downloading);
    progress.set_message(format!(
        "Running mvn dependency:copy-dependencies (resolves ~50 transitive deps of xtable-utilities@{version} \
         into {} -- 3-5 min depending on network and the local ~/.m2 cache state)",
        lib_dir.display()
    ));
    let out = Command::new("mvn")
        .arg("-f")
        .arg(&pom_path)
        .arg("dependency:copy-dependencies")
        .arg(format!("-DoutputDirectory={}", lib_dir.display()))
        .arg("-DincludeScope=runtime")
        .arg("-q")
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "mvn dependency:copy-dependencies for xtable-utilities@{version} failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ))));
    }

    // 5. systemd unit.
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit_body = systemd_unit(&java_bin, &lib_dir, &opts.root_dir);
    let mut f = fs::File::create(&unit_path).await?;
    f.write_all(unit_body.as_bytes()).await?;
    f.sync_all().await?;
    info!(unit = %unit_path.display(), "wrote xtable systemd unit");
    super::systemctl::daemon_reload().await?;

    progress.set_phase(InstallPhase::StartingService);
    progress.set_message(format!("Starting {}", opts.unit_name));
    super::systemctl::enable_now(&opts.unit_name).await?;

    Ok(InstalledService {
        bin_dir: lib_dir,
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
            root_dir: PathBuf::from("/var/lib/computeza/xtable"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    super::service::uninstall_service("xtable", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/xtable");
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-xtable".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: Some(root.join("lib")),
        bin_dir: None,
    }]
}

async fn require_on_path(cmd: &str) -> Result<(), ServiceError> {
    let out = Command::new("which").arg(cmd).output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::Io(std::io::Error::other(format!(
            "`{cmd}` not found on PATH. xtable install requires Maven for the dep-resolve step \
             (`org.apache.xtable:xtable-utilities` ships as a thin JAR on Maven Central; Maven \
             resolves the ~50 transitive deps at install time). \
             Debian / Ubuntu: `apt install maven`. Fedora / RHEL: `dnf install maven`. \
             OpenSUSE: `zypper install maven`. Arch: `pacman -S maven`."
        ))))
    }
}

fn pom_xml(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>local.computeza</groupId>
  <artifactId>xtable-runtime</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>
  <dependencies>
    <dependency>
      <groupId>org.apache.xtable</groupId>
      <artifactId>xtable-utilities</artifactId>
      <version>{version}</version>
    </dependency>
  </dependencies>
</project>
"#
    )
}

fn systemd_unit(java_bin: &Path, lib_dir: &Path, root_dir: &Path) -> String {
    // We launch the utilities runner as a one-shot foreground process
    // pointed at a config file we don't yet generate. v0.1 wires the
    // operator-supplied source/target spec; for v0.0.x the unit
    // installs but won't sync anything useful until the spec is in
    // place. ExecStart shape stays stable so the spec drop-in is
    // additive.
    format!(
        "[Unit]\n\
         Description=Computeza-managed Apache XTable runner\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={java} -cp {lib}/* org.apache.xtable.utilities.RunSync --datasetConfig {root}/datasets.yaml\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ReadWritePaths={root}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        java = java_bin.display(),
        lib = lib_dir.display(),
        root = root_dir.display(),
    )
}
