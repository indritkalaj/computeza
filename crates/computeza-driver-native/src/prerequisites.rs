//! Host-side prerequisite detection and bundle definitions.
//!
//! The goal stated by the project owner: "deliver the dependencies
//! since the hosting OS might not have them installed". A virgin
//! Linux install isn't guaranteed to ship openssl, the Rust toolchain,
//! or a JRE -- yet some Computeza components shell out to them at
//! install time:
//!
//! | Component | Prerequisite | Used for                              |
//! |-----------|--------------|---------------------------------------|
//! | kanidm    | openssl      | self-signed TLS cert at install time  |
//! | kanidm    | cargo        | building kanidmd from crates.io       |
//! | xtable    | java >= 17   | runtime for the xtable runner JAR     |
//! | postgres  | psql         | post-install role bootstrap           |
//!
//! Everything else (tar, gzip, xz, zip extraction; HTTP fetch; sha256;
//! systemd) is either pure Rust (via the `tar`, `flate2`, `liblzma`,
//! `zip`, `reqwest`, `sha2` crates) or comes baked into every supported
//! distro by definition (systemctl).
//!
//! The v0.0.x strategy is **detect + surface**, not **detect + auto
//! install**. Auto-installing arbitrary system packages on an operator's
//! host without consent is exactly the kind of "magic" that erodes
//! trust on enterprise installs; instead we render an actionable
//! one-liner the operator can run themselves. The JRE is the one
//! exception: it's a sandboxed self-contained tarball that drops into
//! `<component_root>/jre/` next to the runner JAR, never touches the
//! system PATH, and gets removed by `uninstall`. That's safe to bundle
//! autonomously.
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
pub const SYSTEM_COMMANDS: &[SystemCommand] = &[
    SystemCommand {
        name: "openssl",
        required_for: "kanidm install (self-signed TLS cert generation)",
        install_hint: "apt-get install -y openssl  (Debian/Ubuntu) | dnf install -y openssl (RHEL family) | zypper install -y openssl (SUSE) | pacman -S --noconfirm openssl (Arch)",
    },
    SystemCommand {
        name: "cargo",
        required_for: "kanidm install (compiles kanidmd from crates.io)",
        install_hint: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y",
    },
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
