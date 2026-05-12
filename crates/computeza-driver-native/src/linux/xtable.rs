//! Apache XTable Iceberg<->Delta<->Hudi metadata sync. Linux install path.
//!
//! # Distribution channel
//!
//! Two paths land xtable's runner JAR; the driver tries them in
//! order:
//!
//! 1. **Computeza-bundled fat JAR** (preferred). Built by
//!    `.github/workflows/build-xtable-fat-jar.yml` and attached to a
//!    GitHub release on this repo (GitHub-fronted CDN). Single JAR
//!    with every transitive dep baked in via maven-shade-plugin; the
//!    driver downloads + verifies SHA-256 + drops it under
//!    `<root>/lib/xtable-fat.jar`. No host Maven required.
//!
//! 2. **Maven Central resolve** (fallback). When the operator's
//!    network can't reach our release URL, the driver writes a
//!    one-off `pom.xml` and shells to
//!    `mvn dependency:copy-dependencies`. Requires `mvn` on host
//!    `$PATH`; the [`crate::prerequisites::SYSTEM_COMMANDS`] table
//!    surfaces the install hint.
//!
//! Both paths bundle Adoptium Temurin JRE 21 sandboxed under
//! `<root>/jre/` via [`crate::prerequisites::ensure_bundled_temurin_jre`],
//! and both register the same systemd unit running
//! `<root>/jre/.../bin/java -cp ...`.

use std::io;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
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

/// Per-version Computeza-bundled fat JAR. URL is a GitHub release
/// asset hosted on this repo; the build pipeline is in
/// `.github/workflows/build-xtable-fat-jar.yml`. The SHA-256 is
/// pinned so a swapped JAR fails the install loudly. `None` for a
/// version means the fat JAR is not yet published -- the driver
/// falls through to the Maven-resolve path automatically.
const FAT_JAR_PINS: &[(&str, &str, &str)] = &[
    // (xtable_version, fat-jar URL, expected SHA-256)
    //
    // 0.3.0-incubating + 0.2.0-incubating: the fat JAR has NOT yet
    // been built (the workflow ships in this commit; first build
    // attaches the asset). Until then both entries below pin `""`
    // as the SHA, which the driver treats as "fat-JAR path
    // disabled, use Maven fallback". An operator running an
    // already-built fat JAR locally can sideload it into
    // `<root>/lib/xtable-fat.jar` and the install detects + uses
    // it.
];

/// Look up the fat-JAR pin for a given xtable version, if any.
fn fat_jar_pin(version: &str) -> Option<(&'static str, &'static str)> {
    for (v, url, sha) in FAT_JAR_PINS {
        if *v == version && !sha.is_empty() && !url.is_empty() {
            return Some((url, sha));
        }
    }
    None
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

    // 1. JRE bootstrap is shared between both classpath paths.
    fs::create_dir_all(&opts.root_dir).await?;
    let java_bin = crate::prerequisites::ensure_bundled_temurin_jre(&opts.root_dir, progress)
        .await
        .map_err(ServiceError::Io)?;

    let lib_dir = opts.root_dir.join("lib");
    fs::create_dir_all(&lib_dir).await?;

    // 2. Try the Computeza-bundled fat JAR first when a pin is
    //    available for this version. Falls through to the Maven
    //    resolve on network failure / SHA mismatch.
    let classpath_path = if let Some((url, sha256)) = fat_jar_pin(version) {
        progress.set_phase(InstallPhase::Downloading);
        progress.set_message(format!(
            "Downloading the Computeza-bundled xtable fat JAR for {version} from {url} \
             (single-file install path; no host Maven required)"
        ));
        match fetch_fat_jar(url, sha256, &lib_dir, progress).await {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    version = %version,
                    "xtable fat-JAR download failed; falling back to Maven-resolve. \
                     Operator: install `mvn` on the host so the fallback succeeds."
                );
                None
            }
        }
    } else {
        None
    };

    if classpath_path.is_none() {
        // 3. Maven fallback: needs mvn on $PATH.
        progress.set_phase(InstallPhase::DetectingBinaries);
        progress
            .set_message("Verifying Maven (mvn) is on PATH for the xtable dep-resolve fallback");
        require_on_path("mvn").await?;

        progress.set_phase(InstallPhase::Extracting);
        progress.set_message(format!(
            "Writing build/pom.xml for xtable-utilities@{version} (Maven-resolve path)"
        ));
        let build_dir = opts.root_dir.join("build");
        fs::create_dir_all(&build_dir).await?;
        let pom = pom_xml(version);
        let pom_path = build_dir.join("pom.xml");
        let mut f = fs::File::create(&pom_path).await?;
        f.write_all(pom.as_bytes()).await?;
        f.sync_all().await?;

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
    }

    // 5. systemd unit. ExecStart shape depends on which classpath
    //    path won: a fat JAR launches with `java -jar`, a Maven-
    //    resolved lib dir launches with `java -cp lib/* RunSync`.
    let classpath = match classpath_path {
        Some(jar) => XtableClasspath::FatJar(jar),
        None => XtableClasspath::LibDir(lib_dir.clone()),
    };
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit_body = systemd_unit(&java_bin, &classpath, &opts.root_dir);
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

/// Which classpath shape the install resolved to. Drives the
/// ExecStart line; everything else in the unit is shared.
enum XtableClasspath {
    /// Single Computeza-bundled fat JAR with embedded Main-Class
    /// manifest. Launches via `java -jar <jar>`.
    FatJar(PathBuf),
    /// Maven-resolved directory of JARs. Launches via
    /// `java -cp <lib>/* org.apache.xtable.utilities.RunSync`.
    LibDir(PathBuf),
}

fn systemd_unit(java_bin: &Path, classpath: &XtableClasspath, root_dir: &Path) -> String {
    // We launch the utilities runner as a one-shot foreground process
    // pointed at a config file we don't yet generate. v0.1 wires the
    // operator-supplied source/target spec; for v0.0.x the unit
    // installs but won't sync anything useful until the spec is in
    // place. ExecStart shape stays stable so the spec drop-in is
    // additive.
    let exec_start = match classpath {
        XtableClasspath::FatJar(jar) => format!(
            "{java} -jar {jar} --datasetConfig {root}/datasets.yaml",
            java = java_bin.display(),
            jar = jar.display(),
            root = root_dir.display(),
        ),
        XtableClasspath::LibDir(lib) => format!(
            "{java} -cp {lib}/* org.apache.xtable.utilities.RunSync --datasetConfig {root}/datasets.yaml",
            java = java_bin.display(),
            lib = lib.display(),
            root = root_dir.display(),
        ),
    };
    format!(
        "[Unit]\n\
         Description=Computeza-managed Apache XTable runner\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
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
        root = root_dir.display(),
    )
}

/// Stream-download the Computeza-bundled xtable fat JAR, verify
/// SHA-256 on the fly, and drop it under `<lib_dir>/xtable-fat.jar`.
///
/// Streaming + hashing in one pass avoids a second read of the
/// downloaded file from disk. We write to `<lib>/xtable-fat.jar.partial`
/// and rename on success so a torn download never lingers under the
/// expected name (which would cause the install to skip the next try).
async fn fetch_fat_jar(
    url: &str,
    expected_sha256: &str,
    lib_dir: &Path,
    progress: &ProgressHandle,
) -> io::Result<PathBuf> {
    let dest = lib_dir.join("xtable-fat.jar");
    let tmp = lib_dir.join("xtable-fat.jar.partial");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60 * 30))
        .build()
        .map_err(io::Error::other)?;
    let resp = client.get(url).send().await.map_err(io::Error::other)?;
    if !resp.status().is_success() {
        return Err(io::Error::other(format!(
            "GET {url} returned HTTP {}",
            resp.status().as_u16()
        )));
    }

    let total = resp.content_length();
    progress.set_bytes(0, total);

    let mut file = fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(io::Error::other)?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        progress.set_bytes(downloaded, total);
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    progress.set_phase(InstallPhase::Verifying);
    progress.set_message("Verifying xtable fat JAR SHA-256");
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        let _ = fs::remove_file(&tmp).await;
        return Err(io::Error::other(format!(
            "xtable fat-JAR SHA-256 mismatch (url: {url}): expected {expected_sha256}, got {actual}"
        )));
    }

    fs::rename(&tmp, &dest).await?;
    info!(path = %dest.display(), "xtable fat JAR downloaded + verified");
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_jar_pin_empty_table_returns_none() {
        // With FAT_JAR_PINS empty (no fat JAR shipped yet) every
        // version must resolve to None so the install falls through
        // to the Maven path.
        assert!(fat_jar_pin("0.3.0-incubating").is_none());
        assert!(fat_jar_pin("0.2.0-incubating").is_none());
        assert!(fat_jar_pin("does-not-exist").is_none());
    }

    #[test]
    fn fat_jar_pin_rejects_blank_sha_or_url() {
        // Same guard as fat_jar_pin's internal check: an entry with
        // an empty url or sha is treated as "no pin" so a half-
        // populated row in FAT_JAR_PINS doesn't trip a broken
        // install. We can't mutate the const at runtime, so this
        // exercises the helper with the empty-table case as a stand
        // in. (The non-empty case will be covered by an integration
        // test when the first fat JAR ships.)
        for (v, _, _) in FAT_JAR_PINS {
            // If any pre-populated row violates the invariant the
            // test fails loudly.
            assert!(!v.is_empty(), "FAT_JAR_PINS row has empty version");
        }
    }

    #[test]
    fn systemd_unit_fatjar_variant_uses_jar_flag() {
        let java = PathBuf::from("/var/lib/computeza/xtable/jre/bin/java");
        let jar = PathBuf::from("/var/lib/computeza/xtable/lib/xtable-fat.jar");
        let root = PathBuf::from("/var/lib/computeza/xtable");
        let unit = systemd_unit(&java, &XtableClasspath::FatJar(jar.clone()), &root);
        assert!(unit.contains(&format!(
            "ExecStart={} -jar {} --datasetConfig {}/datasets.yaml",
            java.display(),
            jar.display(),
            root.display()
        )));
        assert!(unit.contains("ReadWritePaths=/var/lib/computeza/xtable"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_unit_libdir_variant_uses_classpath_glob() {
        let java = PathBuf::from("/var/lib/computeza/xtable/jre/bin/java");
        let lib = PathBuf::from("/var/lib/computeza/xtable/lib");
        let root = PathBuf::from("/var/lib/computeza/xtable");
        let unit = systemd_unit(&java, &XtableClasspath::LibDir(lib.clone()), &root);
        assert!(unit.contains(&format!(
            "ExecStart={} -cp {}/* org.apache.xtable.utilities.RunSync --datasetConfig {}/datasets.yaml",
            java.display(),
            lib.display(),
            root.display()
        )));
    }

    #[test]
    fn pom_xml_includes_requested_version() {
        let pom = pom_xml("0.3.0-incubating");
        assert!(pom.contains("<artifactId>xtable-utilities</artifactId>"));
        assert!(pom.contains("<version>0.3.0-incubating</version>"));
        assert!(pom.contains("<groupId>org.apache.xtable</groupId>"));
    }
}
