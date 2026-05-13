//! Apache XTable Iceberg<->Delta<->Hudi metadata sync. Linux install path.
//!
//! # Distribution channel
//!
//! Apache XTable is an Apache Incubator podling. **Its releases ship
//! source-only** -- no prebuilt JARs land on Maven Central, and no
//! binary release-assets ship on the GitHub release page. The
//! published "releases" are PGP-signed source tarballs intended to be
//! built locally per the standard Apache release model. Both
//! 0.2.0-incubating and 0.3.0-incubating exhibit this: the git tags
//! exist on `github.com/apache/incubator-xtable`, but
//! `repo.maven.apache.org/maven2/org/apache/xtable/xtable-utilities/`
//! returns 404 for every version. Earlier attempts to resolve via
//! `mvn dependency:copy-dependencies` against Maven Central therefore
//! failed for every version we tried -- the artifact simply isn't
//! there.
//!
//! Driver flow on a fresh host:
//!
//! 1. Bundle Adoptium Temurin JRE 21 under `<root>/jre/` via
//!    [`crate::prerequisites::ensure_bundled_temurin_jre`] -- used at
//!    *runtime* to execute the fat JAR.
//! 2. Ensure Maven on `$PATH` (auto-installs via `apt` on Ubuntu
//!    when missing). The `maven` apt package pulls
//!    `default-jdk-headless` as a hard dependency, which provides
//!    the `javac` Maven needs at build time. Our bundled Temurin
//!    JRE is only for runtime.
//! 3. Download source tarball
//!    `github.com/apache/incubator-xtable/archive/refs/tags/<v>.tar.gz`,
//!    extract under `<root>/src/incubator-xtable-<v>/` (cached on
//!    re-run).
//! 4. Run `mvn clean package -DskipTests -B -e` against the source
//!    tree. XTable's `xtable-utilities` module uses
//!    `maven-shade-plugin` to produce a self-contained fat JAR at
//!    `xtable-utilities/target/xtable-utilities-<v>-bundled.jar`
//!    with every transitive dep (iceberg, hudi, delta-core,
//!    parquet, hadoop, aws-sdk, ...) baked in. The build is slow
//!    on first run (5-15 min depending on network -- ~500MB of
//!    transitive Maven Central downloads); progress messages
//!    communicate this.
//! 5. Copy the bundled JAR to `<root>/lib/xtable-utilities-<v>-bundled.jar`
//!    so the rest of the pipeline targets a stable path. The full
//!    source tree stays on disk under `<root>/src/` so a re-install
//!    with the same version short-circuits before mvn runs again.
//! 6. Register a systemd unit running
//!    `<root>/jre/.../bin/java -jar <bundled.jar> --datasetConfig <root>/datasets.yaml`.

use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::info;

use crate::progress::{InstallPhase, ProgressHandle};

use super::service::{InstalledService, ServiceError, Uninstalled};

pub const SERVICE_NAME: &str = "computeza-xtable";
pub const UNIT_NAME: &str = "computeza-xtable.service";
pub const DEFAULT_PORT: u16 = 8090;

/// Pinned versions of Apache XTable (source git tags on
/// `github.com/apache/incubator-xtable`). First entry is the default
/// ("latest"). Source-only -- we build locally with Maven; see the
/// module doc for why Maven Central resolution doesn't work.
///
/// Verify a tag exists at
/// <https://github.com/apache/incubator-xtable/releases> before adding
/// it. The URL pattern is
/// `github.com/apache/incubator-xtable/archive/refs/tags/<tag>.tar.gz`.
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
    /// `src/incubator-xtable-<v>/` (extracted source tree),
    /// `lib/xtable-utilities-<v>-bundled.jar` (built fat JAR),
    /// `datasets.yaml` (operator-owned sync spec).
    pub root_dir: PathBuf,
    /// TCP port the runner binds. Currently informational --
    /// xtable's RunSync doesn't open a port; the field exists for
    /// future versions and for parity with the InstallConfig shape.
    pub port: u16,
    /// systemd unit name.
    pub unit_name: String,
    /// xtable version tag. `None` resolves to `XTABLE_VERSIONS[0]`.
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

pub async fn install(
    opts: InstallOptions,
    progress: &ProgressHandle,
) -> Result<InstalledService, ServiceError> {
    let version = opts.version.as_deref().unwrap_or(XTABLE_VERSIONS[0]);

    // 1. Runtime JRE for systemd to launch the fat JAR with.
    fs::create_dir_all(&opts.root_dir).await?;
    let java_bin = crate::prerequisites::ensure_bundled_temurin_jre(&opts.root_dir, progress)
        .await
        .map_err(ServiceError::Io)?;

    let lib_dir = opts.root_dir.join("lib");
    fs::create_dir_all(&lib_dir).await?;
    let dest_jar = lib_dir.join(format!("xtable-utilities-{version}-bundled.jar"));

    // 2. Short-circuit on re-install. If the bundled JAR for this
    //    exact version is already on disk we skip the multi-minute
    //    source-build step.
    if fs::try_exists(&dest_jar).await.unwrap_or(false) {
        info!(
            jar = %dest_jar.display(),
            "xtable bundled JAR already present; skipping source build"
        );
        progress.set_message(format!(
            "Using cached xtable-utilities-{version}-bundled.jar at {}",
            dest_jar.display()
        ));
    } else {
        // 3. Need Maven (which pulls a JDK as a hard apt dep so we
        //    get javac for free). Auto-install on Ubuntu when
        //    missing, mirroring the postgres driver pattern.
        progress.set_phase(InstallPhase::DetectingBinaries);
        progress.set_message("Verifying Maven (mvn) is on PATH for the xtable source build");
        if let Err(e) = require_on_path("mvn").await {
            progress.set_phase(InstallPhase::Downloading);
            progress.set_message(
                "Auto-installing Maven via apt (one-time; required to build xtable from source)",
            );
            if let Err(install_err) = auto_install_maven().await {
                return Err(ServiceError::Io(std::io::Error::other(format!(
                    "{e}\n\nAuto-install fallback also failed: {install_err}"
                ))));
            }
            require_on_path("mvn").await?;
        }

        // 4. Download + extract the source tarball.
        let src_root = opts.root_dir.join("src");
        fs::create_dir_all(&src_root).await?;
        progress.set_phase(InstallPhase::Downloading);
        progress.set_message(format!(
            "Downloading Apache XTable source tarball for {version} from \
             github.com/apache/incubator-xtable"
        ));
        let src_dir = ensure_xtable_source_extracted(&src_root, version).await?;

        // 5. Build with mvn. xtable-utilities' shade plugin produces
        //    the bundled fat JAR. We invoke from inside the source
        //    tree because xtable's parent pom carries the version
        //    + module wiring; `mvn -f <pom> package` against the
        //    parent works the same way as `cd <src> && mvn package`,
        //    but the latter is closer to how operators reproduce
        //    the build by hand if they need to debug.
        progress.set_phase(InstallPhase::Extracting);
        progress.set_message(format!(
            "Building xtable-utilities@{version} with mvn clean package -DskipTests \
             ({}). First-run downloads ~500MB of transitive Maven Central deps; \
             expect 5-15 min on a fresh ~/.m2 cache.",
            src_dir.display()
        ));
        // -B (batch mode) drops the interactive download progress
        // bar so the captured output is parseable on failure.
        // -e adds the stack trace location to error reports so the
        // diagnostic surface contains enough to act on without
        // re-running with -X.
        // No -q: when the build fails, the BUILD FAILURE banner and
        // per-module error live on stdout; suppressing them leaves
        // the operator with only the irrelevant JVM `sun.misc.Unsafe`
        // warnings on stderr.
        let out = Command::new("mvn")
            .current_dir(&src_dir)
            .arg("clean")
            .arg("package")
            .arg("-DskipTests")
            .arg("-B")
            .arg("-e")
            .output()
            .await?;
        if !out.status.success() {
            return Err(ServiceError::Io(std::io::Error::other(format!(
                "mvn clean package for xtable@{version} failed (exit {:?}). \
                 Source tree at {}.\n\n--- mvn stdout (the diagnostic surface) ---\n{}\n--- mvn stderr (mostly JVM warnings) ---\n{}",
                out.status.code(),
                src_dir.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ))));
        }

        // 6. Locate the freshly-built bundled JAR and copy into lib/.
        //    Path is `<src>/xtable-utilities/target/xtable-utilities-<v>-bundled.jar`
        //    on Apache XTable's current module layout. If the build
        //    succeeded but the JAR isn't where we expect, the
        //    project's shade-plugin output naming has changed --
        //    fall back to a glob to keep working through their
        //    naming variance.
        let built_jar = locate_bundled_jar(&src_dir, version).await?;
        fs::copy(&built_jar, &dest_jar).await?;
        info!(
            from = %built_jar.display(),
            to = %dest_jar.display(),
            "xtable bundled JAR copied into lib/"
        );
    }

    // 7. Stub datasets.yaml. xtable's RunSync requires
    //    `--datasetConfig <file>`; on a fresh install the operator
    //    hasn't supplied one yet, so write an empty stub. The daemon
    //    syncs nothing until the operator replaces this with a real
    //    spec, which is the v0.0.x behaviour we want (install
    //    plumbing today, real sync later). Skip if the operator has
    //    already laid one down to avoid clobbering their work.
    let datasets_path = opts.root_dir.join("datasets.yaml");
    if !fs::try_exists(&datasets_path).await.unwrap_or(false) {
        let stub = "# Apache XTable dataset sync spec. Generated on install.\n\
                    # Replace this stub with your real source/target spec when ready.\n\
                    # See https://xtable.apache.org/docs/setup for the schema.\n\
                    datasets: []\n";
        fs::write(&datasets_path, stub).await?;
        info!(path = %datasets_path.display(), "wrote stub datasets.yaml");
    }

    // 8. systemd unit. Uses the bundled fat JAR's embedded Main-Class
    //    manifest, so the ExecStart is `java -jar <jar>` with no
    //    explicit class name.
    progress.set_phase(InstallPhase::RegisteringService);
    progress.set_message(format!("Registering systemd unit {}", opts.unit_name));
    let unit_path = PathBuf::from("/etc/systemd/system").join(&opts.unit_name);
    let unit_body = systemd_unit(&java_bin, &dest_jar, &opts.root_dir);
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
            "`{cmd}` not found on PATH. xtable install requires Maven to build the \
             upstream source tree (XTable's incubating releases are source-only; no \
             prebuilt JARs are published to Maven Central). \
             Debian / Ubuntu: `apt install maven`. Fedora / RHEL: `dnf install maven`. \
             OpenSUSE: `zypper install maven`. Arch: `pacman -S maven`."
        ))))
    }
}

/// Drive apt to install Maven. Mirrors the postgres driver's
/// auto-install pattern: requires root, surfaces stderr verbatim
/// on failure. Ubuntu-only (matches v0.0.x's supported platform).
/// The `maven` apt package pulls `default-jdk-headless` as a hard
/// dependency, so installing Maven also gives us the javac that
/// `mvn package` needs at build time.
async fn auto_install_maven() -> Result<(), ServiceError> {
    let euid_out = Command::new("id").arg("-u").output().await?;
    let euid: u32 = String::from_utf8_lossy(&euid_out.stdout)
        .trim()
        .parse()
        .unwrap_or(u32::MAX);
    if euid != 0 {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "auto-installing Maven requires root (process is uid {euid}). \
             Restart the operator console under `sudo -E ./target/release/computeza serve ...` \
             OR install Maven manually with `sudo apt install -y maven` and re-submit \
             the install form."
        ))));
    }

    let upd = Command::new("apt-get").arg("update").output().await?;
    if !upd.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "`apt-get update` failed (exit {:?}): {}. Check network egress to archive.ubuntu.com.",
            upd.status.code(),
            String::from_utf8_lossy(&upd.stderr).trim()
        ))));
    }

    let out = Command::new("apt-get")
        .arg("install")
        .arg("-y")
        .arg("maven")
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "`apt-get install -y maven` failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))));
    }
    info!("apt-get install -y maven completed");
    Ok(())
}

/// Download + extract the xtable source tarball into `<src_root>`,
/// returning the path to the extracted source tree
/// (`<src_root>/incubator-xtable-<version>/`). Cached: a re-run that
/// finds an extracted tree with a `pom.xml` reuses it without
/// re-downloading.
async fn ensure_xtable_source_extracted(
    src_root: &Path,
    version: &str,
) -> Result<PathBuf, ServiceError> {
    let extracted = src_root.join(format!("incubator-xtable-{version}"));
    if fs::try_exists(extracted.join("pom.xml"))
        .await
        .unwrap_or(false)
    {
        info!(
            extracted = %extracted.display(),
            "xtable source already extracted; skipping download"
        );
        return Ok(extracted);
    }

    let url = format!(
        "https://github.com/apache/incubator-xtable/archive/refs/tags/{version}.tar.gz"
    );
    let tarball = src_root.join(format!("xtable-{version}.tar.gz"));
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(format!("GET {url}: {e}"))))?;
    if !resp.status().is_success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "GET {url} returned HTTP {}; verify the tag exists at \
             https://github.com/apache/incubator-xtable/tags",
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

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(src_root)
        .status()
        .await?;
    if !status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "tar -xzf {} failed (exit {:?}); the downloaded archive may be \
             incomplete or the tag may not exist",
            tarball.display(),
            status.code()
        ))));
    }
    let _ = fs::remove_file(&tarball).await;

    if !fs::try_exists(extracted.join("pom.xml"))
        .await
        .unwrap_or(false)
    {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "extracted {} but no pom.xml inside -- GitHub's archive layout may have \
             changed; expected incubator-xtable-<version>/pom.xml",
            extracted.display()
        ))));
    }
    Ok(extracted)
}

/// Locate the shade-plugin bundled JAR after a successful build.
/// Apache XTable's current module layout places it at
/// `<src>/xtable-utilities/target/xtable-utilities-<v>-bundled.jar`,
/// but if the project renames its shade output we fall back to a
/// glob over `<src>/xtable-utilities/target/*-bundled.jar` so a
/// minor upstream rename doesn't break the install.
async fn locate_bundled_jar(src_dir: &Path, version: &str) -> Result<PathBuf, ServiceError> {
    let target_dir = src_dir.join("xtable-utilities").join("target");
    let expected = target_dir.join(format!("xtable-utilities-{version}-bundled.jar"));
    if fs::try_exists(&expected).await.unwrap_or(false) {
        return Ok(expected);
    }

    // Fallback: scan target/ for any *-bundled.jar.
    if let Ok(mut rd) = fs::read_dir(&target_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with("-bundled.jar") {
                return Ok(entry.path());
            }
        }
    }

    Err(ServiceError::Io(std::io::Error::other(format!(
        "mvn build succeeded but no bundled JAR found under {} (expected \
         xtable-utilities-{version}-bundled.jar). XTable's shade-plugin output \
         naming may have changed at this version.",
        target_dir.display()
    ))))
}

fn systemd_unit(java_bin: &Path, jar: &Path, root_dir: &Path) -> String {
    // xtable's RunSync exits after one sync cycle. With Type=simple
    // + Restart=on-failure systemd would re-launch immediately and
    // we'd be in a tight CPU loop. The correct shape is
    // Type=oneshot + RemainAfterExit=yes so:
    //   - systemd waits for ExecStart to return (this is what
    //     `systemctl start` expects to complete).
    //   - The unit is considered "active" after a successful exit
    //     (RemainAfterExit) so dependent units are happy.
    //   - There is no restart loop. The operator triggers
    //     subsequent syncs via a systemd timer (v0.1+) or by
    //     re-running `systemctl start computeza-xtable`.
    //
    // Cap the JVM heap at 512 MiB so a colocated postgres or qdrant
    // doesn't OOM. Operators with very large tables override via a
    // systemd drop-in.
    let exec_start = format!(
        "{java} $JAVA_OPTS -jar {jar} --datasetConfig {root}/datasets.yaml",
        java = java_bin.display(),
        jar = jar.display(),
        root = root_dir.display(),
    );
    format!(
        "[Unit]\n\
         Description=Computeza-managed Apache XTable runner\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         Environment=\"JAVA_OPTS=-Xmx512m\"\n\
         RuntimeDirectory=xtable\n\
         RuntimeDirectoryMode=0755\n\
         ExecStart=/bin/sh -c '{exec_start}'\n\
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_uses_jar_flag() {
        let java = PathBuf::from("/var/lib/computeza/xtable/jre/bin/java");
        let jar = PathBuf::from(
            "/var/lib/computeza/xtable/lib/xtable-utilities-0.3.0-incubating-bundled.jar",
        );
        let root = PathBuf::from("/var/lib/computeza/xtable");
        let unit = systemd_unit(&java, &jar, &root);
        assert!(
            unit.contains(&format!(
                "-jar {} --datasetConfig {}/datasets.yaml",
                jar.display(),
                root.display()
            )),
            "ExecStart should invoke `java -jar <jar> --datasetConfig <root>/datasets.yaml`"
        );
        assert!(unit.contains("ReadWritePaths=/var/lib/computeza/xtable"));
        assert!(
            unit.contains("Type=oneshot"),
            "xtable's RunSync exits after one cycle -- unit must be Type=oneshot, not Type=simple"
        );
        assert!(
            unit.contains("RemainAfterExit=yes"),
            "oneshot must remain-after-exit so systemctl is-active reports active"
        );
        assert!(
            !unit.contains("Restart=on-failure"),
            "no restart loop on a oneshot"
        );
        assert!(unit.contains("JAVA_OPTS=-Xmx512m"));
    }

    #[test]
    fn xtable_versions_default_is_latest() {
        assert_eq!(
            XTABLE_VERSIONS[0], "0.3.0-incubating",
            "default version pin should be the latest published Apache XTable tag"
        );
    }
}
