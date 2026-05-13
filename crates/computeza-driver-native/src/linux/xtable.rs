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
/// **Default is 0.2.0-incubating**, not 0.3.0-incubating, because
/// 0.3.0 introduced an `xtable-service` Quarkus module that requires
/// JDK >= 17, while the parent pom still pins
/// `lombok-maven-plugin:1.18.20.0` which doesn't work on JDK 17+
/// (lombok-1.18.20's `com.sun.tools.javac` internals were broken by
/// JDK 17 point releases; fixed upstream in lombok 1.18.30 but
/// XTable's pom hasn't been bumped). No single JDK satisfies both
/// constraints. 0.2.0-incubating has no `xtable-service` module, so
/// the build completes cleanly under JDK 11. Will flip back to 0.3.0
/// once XTable upstream bumps lombok-maven-plugin past 1.18.20.
///
/// 0.3.0 is kept in the list so an operator can opt into it (with
/// manual JDK gymnastics) via the UI version selector.
///
/// Verify a tag exists at
/// <https://github.com/apache/incubator-xtable/releases> before adding
/// it. The URL pattern is
/// `github.com/apache/incubator-xtable/archive/refs/tags/<tag>.tar.gz`.
pub const XTABLE_VERSIONS: &[&str] = &["0.2.0-incubating", "0.3.0-incubating"];

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
        // 3. Need Maven AND a JDK 11. XTable's parent pom pins
        //    lombok-maven-plugin:1.18.20.0, which bundles lombok
        //    1.18.20 (released March 2021). That lombok release was
        //    tested against JDK 8-16; JDK 17+ hits two separate bugs:
        //      - JDK 21: `com.sun.tools.javac.code.TypeTag :: UNKNOWN`
        //        (internals removed in JDK 21)
        //      - JDK 17.0.7+: `Cannot read field "bindingsWhenTrue"
        //        because "currentBindings" is null` (Flow analyzer
        //        internals changed in a point release)
        //        -- fixed upstream in lombok 1.18.30 (issue #3361).
        //    Both are inaccessible without patching XTable's pom.
        //    JDK 11 is the sweet spot: was the current LTS when
        //    lombok 1.18.20 shipped, satisfies XTable's enforcer
        //    rule, and avoids both lombok bugs. The resulting JARs
        //    run fine on our Temurin JRE 21 (Java is
        //    forward-compatible: compile-target 11 + runtime 21 OK).
        progress.set_phase(InstallPhase::DetectingBinaries);
        progress.set_message("Verifying Maven + JDK 11 are present for the xtable source build");
        let need_install =
            require_on_path("mvn").await.is_err() || locate_jdk11().await.is_err();
        if need_install {
            progress.set_phase(InstallPhase::Downloading);
            progress.set_message(
                "Auto-installing Maven + JDK 11 via apt (one-time; required to build xtable from source under a lombok-1.18.20-compatible JDK)",
            );
            auto_install_maven_and_jdk11().await?;
            require_on_path("mvn").await?;
        }
        let jdk11_home = locate_jdk11().await.map_err(|e| {
            ServiceError::Io(std::io::Error::other(format!(
                "could not locate a JDK 11 on the host even after `apt install openjdk-11-jdk-headless`. \
                 lombok-maven-plugin 1.18.20.0 (pinned by xtable's parent pom) doesn't work on JDK 17+. \
                 Manual fix: `sudo apt install -y openjdk-11-jdk-headless`. Underlying: {e}"
            )))
        })?;

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
        // Build under JDK 11. JAVA_HOME drives Maven's toolchain;
        // prepending JDK 11's bin to PATH makes any plugins that
        // shell out to `javac` also pick up the right compiler.
        let jdk11_bin = jdk11_home.join("bin");
        let path_var = std::env::var("PATH").unwrap_or_default();
        let augmented_path = format!("{}:{}", jdk11_bin.display(), path_var);
        // -Dmaven.build.cache.enabled=false disables the
        // maven-build-cache-plugin XTable wires into its parent
        // pom. The cache plugin treats `mvn clean package` as
        // "phases cached, skip" without re-running the package
        // phase, so `target/` ends up empty after the `clean` wipe
        // and no JAR is written. For an installer that runs once
        // and discards intermediate state, a real rebuild every
        // time is cheap and reliable; the cache is a
        // developer-ergonomic feature we don't need.
        let out = Command::new("mvn")
            .current_dir(&src_dir)
            .env("JAVA_HOME", &jdk11_home)
            .env("PATH", &augmented_path)
            .arg("clean")
            .arg("package")
            .arg("-DskipTests")
            .arg("-Dmaven.build.cache.enabled=false")
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
             Debian / Ubuntu: `apt install maven openjdk-11-jdk-headless`. \
             Fedora / RHEL: `dnf install maven java-11-openjdk-devel`. \
             OpenSUSE: `zypper install maven java-11-openjdk-devel`. \
             Arch: `pacman -S maven jdk11-openjdk`. \
             JDK 11 (not the system default JDK) is required because XTable's parent pom \
             pins lombok-maven-plugin:1.18.20.0 which breaks on JDK 17+."
        ))))
    }
}

/// Drive apt to install Maven + JDK 11. Mirrors the postgres
/// driver's auto-install pattern: requires root, surfaces stderr
/// verbatim on failure. Ubuntu-only (matches v0.0.x's supported
/// platform).
///
/// Why JDK 11 specifically: XTable's parent pom pins
/// `lombok-maven-plugin:1.18.20.0`, which bundles lombok 1.18.20
/// (March 2021). That lombok release was tested against JDK 8-16;
/// JDK 17+ hits separate bugs (`TypeTag :: UNKNOWN` on JDK 21,
/// `currentBindings is null` on JDK 17.0.7+) that are inaccessible
/// without patching XTable's pom. JDK 11 was the current LTS when
/// lombok 1.18.20 shipped, satisfies XTable's enforcer rule, and
/// avoids the lombok bugs. The resulting JARs run fine on our
/// Temurin JRE 21 (Java is forward-compatible).
async fn auto_install_maven_and_jdk11() -> Result<(), ServiceError> {
    let euid_out = Command::new("id").arg("-u").output().await?;
    let euid: u32 = String::from_utf8_lossy(&euid_out.stdout)
        .trim()
        .parse()
        .unwrap_or(u32::MAX);
    if euid != 0 {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "auto-installing Maven + JDK 11 requires root (process is uid {euid}). \
             Restart the operator console under `sudo -E ./target/release/computeza serve ...` \
             OR install manually with `sudo apt install -y maven openjdk-11-jdk-headless` \
             and re-submit the install form."
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
        .arg("openjdk-11-jdk-headless")
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "`apt-get install -y maven openjdk-11-jdk-headless` failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))));
    }
    info!("apt-get install -y maven openjdk-11-jdk-headless completed");
    Ok(())
}

/// Locate JAVA_HOME for the host's JDK 11. Probes the canonical
/// `update-alternatives`-style paths Debian / Ubuntu uses for
/// openjdk-11. Returns the directory that contains `bin/javac`.
async fn locate_jdk11() -> Result<PathBuf, ServiceError> {
    // Standard Debian/Ubuntu layout for openjdk-11-jdk-headless on
    // amd64. The `.0` suffix variant shows up on some older point
    // releases; the temurin path covers operators who installed
    // Eclipse Temurin via their own apt repo before computeza ran.
    const CANDIDATES: &[&str] = &[
        "/usr/lib/jvm/java-11-openjdk-amd64",
        "/usr/lib/jvm/java-1.11.0-openjdk-amd64",
        "/usr/lib/jvm/temurin-11-jdk-amd64",
        "/usr/lib/jvm/openjdk-11",
    ];
    for c in CANDIDATES {
        let p = PathBuf::from(c);
        if fs::try_exists(p.join("bin/javac")).await.unwrap_or(false) {
            return Ok(p);
        }
    }
    Err(ServiceError::Io(std::io::Error::other(
        "no JDK 11 found under /usr/lib/jvm/. Install with `sudo apt install -y openjdk-11-jdk-headless`.",
    )))
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
///
/// XTable shuffled module names across releases:
///   - 0.3.0-incubating: `xtable-utilities/target/xtable-utilities-<v>-bundled.jar`
///   - 0.2.0-incubating: the runnable fat JAR lives under a
///     differently-named module (`utilities/`, or the shade output
///     suffix differs)
///
/// We walk the whole source tree's `target/` dirs looking for any
/// `*-bundled.jar`, then prefer the one from a `*utilities*` module
/// when multiple candidates exist (intermediate shaded jars from
/// other modules can also have `-bundled` in the name).
async fn locate_bundled_jar(src_dir: &Path, _version: &str) -> Result<PathBuf, ServiceError> {
    // `find` is in coreutils on every Linux distro we target; the
    // -path glob restricts the walk to `*/target/*-bundled.jar`
    // so we don't pull in spurious matches from source dirs.
    let out = Command::new("find")
        .arg(src_dir)
        .arg("-path")
        .arg("*/target/*-bundled.jar")
        .output()
        .await?;
    if !out.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(format!(
            "find for *-bundled.jar under {} failed (exit {:?}): {}",
            src_dir.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))));
    }
    let candidates: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .collect();

    // Prefer the utilities-module jar (contains the RunSync
    // Main-Class manifest) over intermediate shaded jars from other
    // modules.
    if let Some(jar) = candidates.iter().find(|p| {
        let s = p.to_string_lossy();
        s.contains("/utilities/") || s.contains("/xtable-utilities/")
    }) {
        return Ok(jar.clone());
    }
    if let Some(jar) = candidates.first() {
        return Ok(jar.clone());
    }

    // No `*-bundled.jar` anywhere. maven-shade-plugin may be
    // configured to replace the main jar in place (no -bundled
    // classifier). Fall back to the largest jar under a
    // `*utilities*` target/: a shaded fat jar contains every
    // transitive dep so it's an order of magnitude larger than the
    // un-shaded jar. The "biggest jar in utilities" heuristic
    // reliably finds the runnable one.
    let utilities_jars_out = Command::new("find")
        .arg(src_dir)
        .arg("-path")
        .arg("*utilities*/target/*.jar")
        .arg("-not")
        .arg("-name")
        .arg("*sources.jar")
        .arg("-not")
        .arg("-name")
        .arg("*javadoc.jar")
        .arg("-not")
        .arg("-name")
        .arg("*tests.jar")
        .output()
        .await?;
    if utilities_jars_out.status.success() {
        let mut best: Option<(u64, PathBuf)> = None;
        for line in String::from_utf8_lossy(&utilities_jars_out.stdout).lines() {
            let p = PathBuf::from(line.trim());
            if p.as_os_str().is_empty() {
                continue;
            }
            if let Ok(meta) = fs::metadata(&p).await {
                let size = meta.len();
                if best.as_ref().is_none_or(|(s, _)| size > *s) {
                    best = Some((size, p));
                }
            }
        }
        if let Some((_, jar)) = best {
            return Ok(jar);
        }
    }

    // Diagnostic fallback: surface what JARs the build actually
    // produced so the operator (or future Claude) can pick the
    // right module to target.
    let all_jars = Command::new("find")
        .arg(src_dir)
        .arg("-path")
        .arg("*/target/*.jar")
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    Err(ServiceError::Io(std::io::Error::other(format!(
        "mvn build succeeded but no *-bundled.jar found anywhere under {}. \
         XTable's shade-plugin output naming may have changed at this version.\n\n\
         All JARs produced by the build (for diagnostic purposes):\n{}",
        src_dir.display(),
        if all_jars.is_empty() {
            "(none found -- the build may not have produced any jars)".to_string()
        } else {
            all_jars
        }
    ))))
}

/// Fully-qualified class name of XTable's RunSync entry point.
/// Stable across 0.2.0 and 0.3.0-incubating; only used because the
/// shade-plugin config in `xtable-utilities` doesn't write a
/// Main-Class manifest entry, so `java -jar` fails with `no main
/// manifest attribute`. `java -cp <jar> <classname>` is the
/// upstream-documented invocation (see xtable.apache.org/docs/setup).
const RUN_SYNC_CLASS: &str = "org.apache.xtable.utilities.RunSync";

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
    //
    // `-cp <jar> <classname>` not `-jar <jar>`: xtable's shade-plugin
    // doesn't write a Main-Class manifest entry, so `java -jar`
    // fails with "no main manifest attribute". Naming RunSync
    // explicitly matches the upstream-documented invocation.
    let exec_start = format!(
        "{java} $JAVA_OPTS -cp {jar} {cls} --datasetConfig {root}/datasets.yaml",
        java = java_bin.display(),
        jar = jar.display(),
        cls = RUN_SYNC_CLASS,
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
    fn systemd_unit_uses_cp_with_runsync_class() {
        let java = PathBuf::from("/var/lib/computeza/xtable/jre/bin/java");
        let jar = PathBuf::from(
            "/var/lib/computeza/xtable/lib/xtable-utilities-0.2.0-incubating-bundled.jar",
        );
        let root = PathBuf::from("/var/lib/computeza/xtable");
        let unit = systemd_unit(&java, &jar, &root);
        assert!(
            unit.contains(&format!(
                "-cp {} org.apache.xtable.utilities.RunSync --datasetConfig {}/datasets.yaml",
                jar.display(),
                root.display()
            )),
            "ExecStart should invoke `java -cp <jar> RunSync ...` -- xtable's \
             shade-plugin omits Main-Class so `java -jar` won't work"
        );
        assert!(
            !unit.contains(&format!("-jar {}", jar.display())),
            "must NOT use -jar -- the shaded jar lacks a Main-Class manifest entry"
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
    fn xtable_versions_default_is_0_2_0() {
        // 0.2.0 not 0.3.0: 0.3.0 added an xtable-service Quarkus
        // module requiring JDK 17+ while the parent pom still pins
        // a lombok-maven-plugin that breaks on JDK 17+. Flip once
        // XTable upstream bumps the lombok pin. See XTABLE_VERSIONS
        // docstring.
        assert_eq!(
            XTABLE_VERSIONS[0], "0.2.0-incubating",
            "default must stay on 0.2.0 until XTable upstream fixes the lombok/quarkus JDK collision"
        );
    }
}
