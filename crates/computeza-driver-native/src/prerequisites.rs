//! Host-side prerequisite detection and bundle definitions.
//!
//! The goal stated by the project owner: "deliver the dependencies
//! since the hosting OS might not have them installed". A virgin
//! Linux install isn't guaranteed to ship openssl, the Rust toolchain,
//! or a JRE -- yet some Computeza components shell out to them at
//! install time:
//!
//! | Component | Prerequisite | Used for                              | Strategy             |
//! |-----------|--------------|---------------------------------------|----------------------|
//! | kanidm    | rcgen        | self-signed TLS cert at install time  | pure Rust (no host)  |
//! | kanidm    | cargo        | building kanidmd from crates.io       | installed (auto)     |
//! | xtable    | java >= 17   | runtime for the xtable runner JAR     | bundled (auto, JRE)  |
//! | postgres  | psql         | post-install role bootstrap           | shipped in bundle    |
//!
//! Everything else (tar, gzip, xz, zip extraction; HTTP fetch; sha256;
//! systemd) is either pure Rust (via the `tar`, `flate2`, `liblzma`,
//! `zip`, `reqwest`, `sha2` crates) or comes baked into every supported
//! distro by definition (systemctl).
//!
//! The v0.0.x strategy is **detect + surface** for small, near-universal
//! host commands the operator likely already has (openssl) and
//! **detect + autonomous system-wide install** for the rare large
//! dependencies that don't make sense to ask the operator to install
//! themselves (a Rust toolchain to build kanidm, a JRE to run the
//! xtable runner JAR).
//!
//! - **Rust toolchain**: when `cargo` is missing, the driver installs
//!   a complete toolchain into `/var/lib/computeza/toolchain/rust/`
//!   and symlinks `cargo`, `rustc`, and `rustup` into `/usr/local/bin/`
//!   so the operator (and any future component install) can use them
//!   transparently. See [`ensure_rust_toolchain`].
//! - **JRE**: the Temurin JRE drops sandboxed into
//!   `<component_root>/jre/` next to the xtable runner JAR -- it's
//!   internal plumbing for one component, not a general tool the
//!   operator would invoke directly. See [`TEMURIN_JRE_21_X86_64_LINUX`].
//!
//! See AGENTS.md "Host prerequisites" for the operator-facing story.

use std::path::PathBuf;

use crate::fetch::{ArchiveKind, Bundle};

/// A system command that some driver shells out to. `check()` returns
/// whether the command is on `$PATH` so the wizard can surface a
/// friendly hint.
#[derive(Clone, Copy, Debug)]
pub struct SystemCommand {
    /// Command name, e.g. `"openssl"`. Looked up via `which`-style
    /// resolution against the runtime `$PATH`.
    pub name: &'static str,
    /// Which Computeza component(s) need it. Free-form, surfaced in
    /// the prerequisite-missing UI hint.
    pub required_for: &'static str,
    /// Human-readable one-liner the operator can paste to install it.
    /// Detection logic should NOT run this -- it's display-only.
    pub install_hint: &'static str,
}

/// The fixed table of host commands Computeza shells out to.
///
/// **No entry currently surfaces as a hard host prereq.** The table
/// exists as the registration point for future host commands; today
/// the install path is fully self-bootstrapping:
///
/// - `cargo`: installed by [`ensure_rust_toolchain`] into
///   `/var/lib/computeza/toolchain/rust` when missing, then symlinked
///   onto `/usr/local/bin/`.
/// - `openssl`: no longer needed -- the kanidm TLS cert step now uses
///   the pure-Rust `rcgen` crate instead of shelling out to
///   `openssl req`.
/// - `psql`: shipped inside the PostgreSQL bundle Computeza downloads.
/// - `java`: planned to install via [`TEMURIN_JRE_21_X86_64_LINUX`]
///   when xtable wiring lands.
///
/// The `psql` and `java` rows below are informational only -- they
/// document the install-time bundling so the wizard's prereq banner
/// (which checks against this table) does not light up.
pub const SYSTEM_COMMANDS: &[SystemCommand] = &[
    SystemCommand {
        name: "psql",
        required_for: "postgres install (post-install role bootstrap)",
        install_hint: "shipped in the postgres bundle Computeza downloads; this entry is informational",
    },
    SystemCommand {
        name: "java",
        required_for: "xtable install (JRE runtime for the runner JAR)",
        install_hint: "Computeza will auto-install Adoptium Temurin JRE 21 into <root_dir>/jre during xtable install -- no operator action needed",
    },
    SystemCommand {
        name: "mvn",
        required_for: "xtable install (Maven resolves the runner JAR + its ~50 transitive deps from Maven Central at install time)",
        install_hint: "apt-get install -y maven  (Debian/Ubuntu) | dnf install -y maven (RHEL family) | zypper install -y maven (SUSE) | pacman -S --noconfirm maven (Arch)",
    },
];

/// Adoptium Temurin JRE 21 (LTS) bundle. Used by the xtable install
/// path to lay down a sandboxed JRE under `<root_dir>/jre/` so a virgin
/// Linux host without a system Java still works.
///
/// JRE-only (not full JDK) -- ~50MB compressed. Verified May 2026
/// against the adoptium/temurin21-binaries GitHub release.
///
/// This is intentionally a plain [`Bundle`] (not a SystemCommand) so
/// drivers that need a JRE can reuse the existing
/// [`crate::fetch::fetch_and_extract`] machinery. After extraction the
/// JRE lives at `<root_dir>/jre/jdk-21.0.11+10-jre/bin/java`.
pub const TEMURIN_JRE_21_X86_64_LINUX: Bundle = Bundle {
    version: "21.0.11+10",
    url: "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.11%2B10/OpenJDK21U-jre_x64_linux_hotspot_21.0.11_10.tar.gz",
    kind: ArchiveKind::TarGz,
    sha256: None,
    bin_subpath: "jdk-21.0.11+10-jre/bin",
};

/// Adoptium Temurin JRE 25 (LTS, GA October 2025). Used by Trino,
/// which requires Java 22+ for DYNAMIC catalog management (Trino
/// 459+) and Java 23+ for the latest releases. Sitting on Java 25
/// LTS keeps Trino on a long-supported runtime without forcing
/// xtable off its established Java 21 bundle.
///
/// JRE-only (~55MB compressed). Extracted layout:
/// `<root>/jre/jdk-25+36-jre/bin/java`.
pub const TEMURIN_JRE_25_X86_64_LINUX: Bundle = Bundle {
    version: "25+36",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25%2B36/OpenJDK25U-jre_x64_linux_hotspot_25_36.tar.gz",
    kind: ArchiveKind::TarGz,
    sha256: None,
    bin_subpath: "jdk-25+36-jre/bin",
};

/// Result of probing a single [`SystemCommand`] on the host.
#[derive(Clone, Debug)]
pub struct CommandStatus {
    /// The probed command.
    pub command: SystemCommand,
    /// Absolute path on `$PATH`, or `None` if not found.
    pub resolved_path: Option<PathBuf>,
}

impl CommandStatus {
    /// Convenience: whether the command is present on the host.
    #[must_use]
    pub fn present(&self) -> bool {
        self.resolved_path.is_some()
    }
}

/// Probe every command in [`SYSTEM_COMMANDS`] against the current
/// `$PATH`. Pure read-only: no shell execution, no network. Safe to
/// call from any handler.
#[must_use]
pub fn check_all() -> Vec<CommandStatus> {
    SYSTEM_COMMANDS
        .iter()
        .copied()
        .map(|c| CommandStatus {
            command: c,
            resolved_path: which_on_path(c.name),
        })
        .collect()
}

/// Probe a single named command. Mirrors the `which` shell builtin:
/// walks `$PATH` and returns the first entry whose `<dir>/<name>`
/// (plus `.exe` on Windows) is a file.
#[must_use]
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exe_suffixes: &[&str] = if cfg!(target_os = "windows") {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suffix in exe_suffixes {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Pinned URL for the official `rustup-init` x86_64-linux static
/// binary. This is the same binary `sh.rustup.rs` downloads behind
/// the scenes; running it directly skips the shell wrapper and lets
/// us hand the right env vars through without piping through bash.
///
/// Pin is `static.rust-lang.org` -- Rust's official CDN. No checksum
/// is recorded because the binary is signed by the Rust project's
/// release infrastructure (verified at runtime by rustup itself when
/// it pulls toolchain components); a side-channel pin would not add
/// real protection without also pinning the toolchain manifest.
#[cfg(target_os = "linux")]
const RUSTUP_INIT_URL_X86_64_LINUX: &str =
    "https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init";

/// Shared root for Computeza-managed toolchains. Each toolchain lives
/// in its own subdir (`rust/`, `jre/`, ...). Not per-component so that
/// re-installing or installing a second component that needs the same
/// toolchain doesn't re-download it.
#[cfg(target_os = "linux")]
const COMPUTEZA_TOOLCHAIN_ROOT: &str = "/var/lib/computeza/toolchain";

/// Where we register cargo / rustc / rustup symlinks. `/usr/local/bin`
/// is on every Linux distro's default `$PATH`, so a fresh login shell
/// (or any non-shell process that consults `$PATH`) sees the bundled
/// tools immediately.
#[cfg(target_os = "linux")]
const SYSTEM_BIN_DIR: &str = "/usr/local/bin";

/// Rust binaries we expose on `$PATH` after a successful bootstrap.
/// The minimal rustup profile ships exactly these three; if upstream
/// drops one we skip it silently.
#[cfg(target_os = "linux")]
const RUST_TOOLS_ON_PATH: &[&str] = &["cargo", "rustc", "rustup"];

/// Ensure a usable `cargo` binary is available, returning its absolute
/// path. Resolution order:
///
/// 1. **`cargo` on `$PATH`** -- the operator (or a previous Computeza
///    install) already has a working toolchain. Use it directly.
/// 2. **Previous Computeza-managed toolchain cache hit** at
///    `/var/lib/computeza/toolchain/rust/cargo/bin/cargo`. Avoids
///    re-running rustup-init on subsequent installs. The symlinks on
///    `/usr/local/bin/` are re-asserted (idempotent) so they survive
///    a manual `rm /usr/local/bin/cargo`.
/// 3. **Bootstrap a system-wide toolchain.** Downloads the official
///    [`RUSTUP_INIT_URL_X86_64_LINUX`] binary, `chmod +x` it, and runs
///    it with `CARGO_HOME=/var/lib/computeza/toolchain/rust/cargo`,
///    `RUSTUP_HOME=/var/lib/computeza/toolchain/rust/rustup`,
///    `--no-modify-path --profile minimal --default-toolchain stable
///    -y`. After install, symlinks `cargo`, `rustc`, `rustup` into
///    `/usr/local/bin/` so the operator (and any future component
///    install) sees them on `$PATH` immediately -- no re-login needed.
///
/// Why system-wide and not sandboxed per-component:
///
/// - Several components may eventually need a Rust toolchain. Sharing
///   one install avoids re-downloading ~500MB per install.
/// - The operator may want to use `cargo` themselves (debugging,
///   building tools). Putting it on `$PATH` is the least surprising
///   behavior. We do NOT modify shell rc files (`~/.bashrc`, etc.) --
///   the `/usr/local/bin/` symlinks are enough and reverse cleanly.
/// - Uninstall semantics stay simple: removing one component does NOT
///   remove the shared toolchain. Use `computeza` (v0.1+ "purge
///   toolchains" action) or `rm -rf /var/lib/computeza/toolchain` +
///   `rm /usr/local/bin/{cargo,rustc,rustup}` to fully clean.
///
/// Linux-only in v0.0.x: hardcoded for x86_64-unknown-linux-gnu.
#[cfg(target_os = "linux")]
pub async fn ensure_rust_toolchain(
    progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    // Tier 1: host has cargo on $PATH (could be apt-installed, or a
    // previous run of this very function via the /usr/local/bin
    // symlinks).
    if let Some(p) = which_on_path("cargo") {
        tracing::info!(
            cargo = %p.display(),
            "cargo bootstrap: using cargo already on $PATH; \
             skipping the rustup-init download / install"
        );
        return Ok(p);
    }

    let rust_root = PathBuf::from(COMPUTEZA_TOOLCHAIN_ROOT).join("rust");
    let cargo_bin = rust_root.join("cargo").join("bin").join("cargo");

    // Tier 2: cache hit from a previous Computeza-managed install,
    // but the /usr/local/bin symlinks may have been removed -- re-assert
    // them so the operator's shell can find cargo again.
    if cargo_bin.is_file() {
        tracing::info!(
            cargo = %cargo_bin.display(),
            "cargo bootstrap: previously-installed toolchain found at \
             /var/lib/computeza/toolchain/rust; re-asserting /usr/local/bin symlinks"
        );
        register_rust_on_path(&rust_root).await?;
        return Ok(cargo_bin);
    }

    // Tier 3: fresh install.
    tokio::fs::create_dir_all(&rust_root).await?;
    progress.set_message(
        "cargo not on $PATH; installing Rust into /var/lib/computeza/toolchain/rust \
         and registering on /usr/local/bin so future installs (and the operator) \
         can use it transparently",
    );

    let rustup_init_path = rust_root.join("rustup-init");
    download_to(RUSTUP_INIT_URL_X86_64_LINUX, &rustup_init_path).await?;

    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(&rustup_init_path).await?.permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(&rustup_init_path, perms).await?;

    progress.set_message(
        "Running rustup-init --no-modify-path --profile minimal --default-toolchain stable \
         (downloads ~250MB of toolchain; ~3-5 min on first run)",
    );

    let cargo_home = rust_root.join("cargo");
    let rustup_home = rust_root.join("rustup");
    let out = tokio::process::Command::new(&rustup_init_path)
        .arg("--no-modify-path")
        .arg("--default-toolchain")
        .arg("stable")
        .arg("--profile")
        .arg("minimal")
        .arg("-y")
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .output()
        .await?;

    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "rustup-init exited with status {:?}; stderr:\n{}\n\
             Re-run the install after fixing the underlying issue \
             (network access to static.rust-lang.org, ~500MB free under {}).",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
            rust_root.display(),
        )));
    }

    if !cargo_bin.is_file() {
        return Err(std::io::Error::other(format!(
            "rustup-init completed but the cargo binary is missing at {}. \
             Inspect /var/lib/computeza/toolchain/rust for partial state; \
             delete it and re-run install to retry.",
            cargo_bin.display()
        )));
    }

    progress.set_message(
        "Registering cargo, rustc, rustup on /usr/local/bin so the operator can use them",
    );
    register_rust_on_path(&rust_root).await?;

    tracing::info!(
        cargo = %cargo_bin.display(),
        "cargo bootstrap: toolchain installed and registered on /usr/local/bin; \
         operator can now run `cargo`, `rustc`, `rustup` directly from any shell"
    );
    progress.set_message(format!(
        "Rust toolchain ready ({}); kanidm install will compile through this",
        cargo_bin.display()
    ));
    Ok(cargo_bin)
}

/// Non-Linux stub. The bundled-toolchain path is x86_64-linux-only in
/// v0.0.x because every downstream component (kanidm, ...) is itself
/// Linux-only. Calling this on macOS / Windows is a programming
/// error -- the caller should have been gated off by `guard_supported_os`.
#[cfg(not(target_os = "linux"))]
pub async fn ensure_rust_toolchain(
    _progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    Err(std::io::Error::other(
        "ensure_rust_toolchain: bundled Rust toolchain is x86_64-linux-only in v0.0.x; \
         macOS and Windows install paths are not reachable through the wizard",
    ))
}

/// Ensure a usable JRE 21 is available under `<component_root>/jre`,
/// returning the absolute path to the bundled `java` binary.
///
/// Mirrors the [`ensure_rust_toolchain`] pattern but with key
/// differences:
///
/// - **Sandboxed, not system-wide.** The JRE is internal plumbing for
///   one component (xtable, today) -- not a tool the operator runs
///   themselves -- so we drop it under the component root and never
///   register it on `$PATH`. Removing the component root removes the
///   JRE with it.
/// - **No host-PATH fallback.** Detecting a usable host JRE (parsing
///   `java -version` and version-comparing) is fragile across distros;
///   we always use the bundled copy to keep the version exactly
///   pinned and the install reproducible.
///
/// Idempotent: re-running on an existing install is a no-op (the
/// presence of `<root>/jre/.../bin/java` is the cache marker).
#[cfg(target_os = "linux")]
pub async fn ensure_bundled_temurin_jre(
    component_root: &std::path::Path,
    progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    ensure_bundled_temurin_jre_inner(component_root, &TEMURIN_JRE_21_X86_64_LINUX, "21", progress)
        .await
}

/// Java 25 LTS variant of [`ensure_bundled_temurin_jre`]. Trino 459+
/// needs Java 22+ for DYNAMIC catalog management; we go straight to
/// the next LTS so the runtime is supported until 2030+.
#[cfg(target_os = "linux")]
pub async fn ensure_bundled_temurin_jre_25(
    component_root: &std::path::Path,
    progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    ensure_bundled_temurin_jre_inner(component_root, &TEMURIN_JRE_25_X86_64_LINUX, "25", progress)
        .await
}

#[cfg(target_os = "linux")]
async fn ensure_bundled_temurin_jre_inner(
    component_root: &std::path::Path,
    bundle: &Bundle,
    version_label: &str,
    progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    let jre_root = component_root.join("jre");
    let expected_bin = jre_root.join(bundle.bin_subpath).join("java");
    if expected_bin.is_file() {
        tracing::info!(
            java = %expected_bin.display(),
            "temurin bootstrap: using previously-bundled JRE at <root>/jre"
        );
        return Ok(expected_bin);
    }

    tokio::fs::create_dir_all(&jre_root).await?;
    progress.set_message(format!(
        "Bundling Adoptium Temurin JRE {version_label} into <component-root>/jre \
         (one-time download, ~50MB, sandboxed -- not registered on system PATH)"
    ));

    // Stream-download the tarball.
    let tarball_path = jre_root.join("temurin-jre.tar.gz");
    download_to(bundle.url, &tarball_path).await?;

    // Use system tar on Linux for the same reason fetch.rs does:
    // the Rust tar crate trips on some Temurin tarball entries
    // (long paths, hardlinks) and surfaces opaque errors.
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(&jre_root)
        .status()
        .await?;
    if !status.success() {
        // Fall back to the Rust crate if system tar isn't available.
        let bytes = tokio::fs::read(&tarball_path).await?;
        let cursor = std::io::Cursor::new(&bytes[..]);
        let gz = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(gz);
        archive.unpack(&jre_root).map_err(|e| {
            std::io::Error::other(format!(
                "unpacking Temurin JRE tarball into {}: {e}",
                jre_root.display()
            ))
        })?;
    }
    // Drop the tarball -- the unpacked tree is what we keep.
    let _ = tokio::fs::remove_file(&tarball_path).await;

    if !expected_bin.is_file() {
        return Err(std::io::Error::other(format!(
            "Temurin JRE extracted but java binary missing at {}. \
             Inspect <root>/jre for partial state; delete it and re-run install to retry.",
            expected_bin.display()
        )));
    }
    tracing::info!(
        java = %expected_bin.display(),
        version = version_label,
        "temurin bootstrap: JRE ready"
    );
    Ok(expected_bin)
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub async fn ensure_bundled_temurin_jre(
    _component_root: &std::path::Path,
    _progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    Err(std::io::Error::other(
        "ensure_bundled_temurin_jre: bundled Temurin JRE is x86_64-linux-only in v0.0.x",
    ))
}

/// Non-Linux stub for the Java 25 LTS variant.
#[cfg(not(target_os = "linux"))]
pub async fn ensure_bundled_temurin_jre_25(
    _component_root: &std::path::Path,
    _progress: &crate::progress::ProgressHandle,
) -> Result<PathBuf, std::io::Error> {
    Err(std::io::Error::other(
        "ensure_bundled_temurin_jre_25: bundled Temurin JRE is x86_64-linux-only in v0.0.x",
    ))
}

/// Symlink `cargo` / `rustc` / `rustup` from the Computeza toolchain
/// into `/usr/local/bin/` so the operator's shell can find them on
/// `$PATH`. Idempotent: re-running over existing symlinks is a no-op;
/// existing symlinks pointing somewhere else are atomically repointed
/// at our toolchain.
///
/// We deliberately do NOT prefix with `computeza-`: unlike component
/// CLIs (where `computeza-psql` avoids colliding with a distro psql),
/// Rust tools have widely-known names that scripts, build systems,
/// and IDEs look up by their canonical name. A `computeza-cargo`
/// symlink would not help anyone.
#[cfg(target_os = "linux")]
async fn register_rust_on_path(rust_root: &std::path::Path) -> Result<(), std::io::Error> {
    let cargo_bin_dir = rust_root.join("cargo").join("bin");
    let system_bin = PathBuf::from(SYSTEM_BIN_DIR);
    tokio::fs::create_dir_all(&system_bin).await?;

    for tool in RUST_TOOLS_ON_PATH {
        let target = cargo_bin_dir.join(tool);
        if !tokio::fs::try_exists(&target).await.unwrap_or(false) {
            tracing::debug!(
                tool = tool,
                "skipping {tool} symlink: not present in toolchain bin dir (minimal profile)"
            );
            continue;
        }
        let link = system_bin.join(tool);
        // Atomic re-link: write to a tempname, then rename over the
        // existing entry. tokio::fs::rename overwrites on unix.
        let tmp = link.with_extension("computeza-tmp");
        let _ = tokio::fs::remove_file(&tmp).await;
        symlink_async(&target, &tmp).await?;
        tokio::fs::rename(&tmp, &link).await?;
        tracing::info!(
            link = %link.display(),
            target = %target.display(),
            "registered {tool} on system $PATH via /usr/local/bin symlink"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn symlink_async(
    target: &std::path::Path,
    link: &std::path::Path,
) -> Result<(), std::io::Error> {
    let t = target.to_path_buf();
    let l = link.to_path_buf();
    tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(t, l))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
}

/// Download a URL to a destination path. Buffers the whole body in
/// memory then writes once -- fine for the small (~10MB) rustup-init
/// binary; do not use this for large bundles (those go through
/// [`crate::fetch::download_stream`] for streaming + progress).
#[cfg(target_os = "linux")]
async fn download_to(url: &str, dest: &std::path::Path) -> Result<(), std::io::Error> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| std::io::Error::other(format!("HTTP GET {url} failed: {e}")))?
        .error_for_status()
        .map_err(|e| std::io::Error::other(format!("HTTP GET {url} returned error: {e}")))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| std::io::Error::other(format!("reading response body from {url}: {e}")))?;
    tokio::fs::write(dest, &bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_commands_table_is_unique_by_name() {
        let mut names: Vec<_> = SYSTEM_COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate command name in table");
    }

    #[test]
    fn check_all_returns_one_status_per_command() {
        let statuses = check_all();
        assert_eq!(statuses.len(), SYSTEM_COMMANDS.len());
    }

    #[test]
    fn temurin_bundle_is_well_formed() {
        assert!(TEMURIN_JRE_21_X86_64_LINUX.url.starts_with("https://"));
        assert!(TEMURIN_JRE_21_X86_64_LINUX.url.ends_with(".tar.gz"));
        assert_eq!(TEMURIN_JRE_21_X86_64_LINUX.kind, ArchiveKind::TarGz);
    }
}
