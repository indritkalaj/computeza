//! Host operating system detection.
//!
//! The install wizard runs the same Computeza binary on any OS that
//! builds Rust, but the data-plane install drivers only support
//! systemd-based Linux on x86_64 for v0.0.x. This module surfaces what
//! the host actually is so the UI can prominently warn operators on
//! Windows / macOS / Alpine / aarch64 to switch to a supported
//! environment before attempting to install anything.

#[cfg(target_os = "linux")]
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Coarse OS family. Drives the "is this supported?" branching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsFamily {
    /// Linux, with the specific distro identified in `OsInfo::distro_id`.
    Linux,
    /// macOS / Darwin.
    MacOs,
    /// Windows.
    Windows,
    /// Anything else (FreeBSD, illumos, ...).
    Other,
}

/// Snapshot of the host environment. Cheap to compute -- callers can
/// re-detect on every page render without measurable overhead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OsInfo {
    /// Coarse OS family.
    pub family: OsFamily,
    /// `/etc/os-release` `ID` field on Linux (e.g. "ubuntu", "fedora",
    /// "alpine"). `None` on non-Linux or when /etc/os-release is
    /// missing.
    pub distro_id: Option<String>,
    /// `/etc/os-release` `PRETTY_NAME` field on Linux (e.g.
    /// "Ubuntu 24.04.1 LTS"). On macOS / Windows this is a similar
    /// human-readable string built from std::env / system calls.
    pub distro_name: Option<String>,
    /// `uname -m` result: "x86_64", "aarch64", etc.
    pub arch: String,
    /// Whether this host can run the v0.0.x install drivers. True
    /// only for systemd-based Linux distros on x86_64.
    pub supported: bool,
    /// Operator-facing reason when `supported = false`. Includes a
    /// short pointer at the supported environments.
    pub unsupported_reason: Option<String>,
}

/// Linux distros we know the drivers work on. Loose check: matches
/// against `os-release` `ID` and `ID_LIKE`. Anything else falls
/// through to "untested -- proceed at your own risk" rather than a
/// hard block, because most systemd + glibc distros end up working
/// regardless of branding.
pub const SUPPORTED_LINUX_DISTROS: &[&str] = &[
    "ubuntu",
    "debian",
    "fedora",
    "rhel",
    "centos",
    "rocky",
    "almalinux",
    "opensuse-leap",
    "opensuse-tumbleweed",
    "sles",
    "arch",
    "manjaro",
];

/// Distros we know don't work. Today: Alpine (musl + OpenRC) and
/// anything that explicitly opts out of systemd.
pub const UNSUPPORTED_LINUX_DISTROS: &[&str] = &["alpine", "gentoo", "void"];

/// Detect the host environment.
pub fn detect() -> OsInfo {
    let arch = std::env::consts::ARCH.to_string();

    #[cfg(target_os = "linux")]
    {
        detect_linux(arch)
    }
    #[cfg(target_os = "macos")]
    {
        OsInfo {
            family: OsFamily::MacOs,
            distro_id: None,
            distro_name: Some("macOS".into()),
            arch,
            supported: false,
            unsupported_reason: Some(unsupported_message("macOS")),
        }
    }
    #[cfg(target_os = "windows")]
    {
        OsInfo {
            family: OsFamily::Windows,
            distro_id: None,
            distro_name: Some("Windows".into()),
            arch,
            supported: false,
            unsupported_reason: Some(unsupported_message("Windows")),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        OsInfo {
            family: OsFamily::Other,
            distro_id: None,
            distro_name: Some(std::env::consts::OS.to_string()),
            arch,
            supported: false,
            unsupported_reason: Some(unsupported_message(std::env::consts::OS)),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_linux(arch: String) -> OsInfo {
    let (distro_id, distro_name) = read_os_release();
    let systemd_ok = Path::new("/run/systemd/system").exists();
    let arch_ok = arch == "x86_64";
    let id_lower = distro_id.as_deref().unwrap_or("").to_ascii_lowercase();
    let known_bad = UNSUPPORTED_LINUX_DISTROS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(&id_lower));

    let mut reasons: Vec<String> = Vec::new();
    if !systemd_ok {
        reasons.push(
            "systemd is not the init system on this host (no /run/systemd/system). \
             v0.0.x install drivers register systemd units; OpenRC / runit / s6 are \
             not yet supported."
                .into(),
        );
    }
    if !arch_ok {
        reasons.push(format!(
            "host arch is {arch}; v0.0.x bundle pins target x86_64 only. aarch64 / armv7 / \
             riscv64 builds land in a follow-up."
        ));
    }
    if known_bad {
        reasons.push(format!(
            "{id_lower} is on the known-unsupported list (typically musl + OpenRC). Use \
             a systemd + glibc distro: Ubuntu / Debian / Fedora / RHEL / OpenSUSE / Arch."
        ));
    }

    let supported = reasons.is_empty();
    OsInfo {
        family: OsFamily::Linux,
        distro_id,
        distro_name,
        arch,
        supported,
        unsupported_reason: if supported {
            None
        } else {
            Some(reasons.join(" "))
        },
    }
}

#[cfg(target_os = "linux")]
fn read_os_release() -> (Option<String>, Option<String>) {
    // /etc/os-release is the freedesktop.org spec; /usr/lib/os-release is
    // the symlink fallback some distros use.
    let candidates = ["/etc/os-release", "/usr/lib/os-release"];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(path) {
            let id = parse_os_release_field(&text, "ID");
            let pretty = parse_os_release_field(&text, "PRETTY_NAME").or_else(|| {
                parse_os_release_field(&text, "NAME").map(|n| {
                    parse_os_release_field(&text, "VERSION")
                        .map(|v| format!("{n} {v}"))
                        .unwrap_or(n)
                })
            });
            return (id, pretty);
        }
    }
    (None, None)
}

#[cfg(target_os = "linux")]
fn parse_os_release_field(body: &str, key: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(value) = rest.strip_prefix('=') {
                // Values may be quoted with single or double quotes.
                let trimmed = value
                    .trim()
                    .trim_matches(|c: char| c == '"' || c == '\'')
                    .to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn unsupported_message(detected: &str) -> String {
    format!(
        "Detected {detected}. v0.0.x installs the data-plane components only on \
         systemd-based Linux distros (Ubuntu, Debian, Fedora, RHEL/CentOS Stream, \
         Rocky Linux, AlmaLinux, OpenSUSE Leap/Tumbleweed, SLES, Arch, Manjaro). \
         Switch to one of those, or run the operator console here and point the \
         install wizard at a remote Linux host once the multi-host install path \
         lands in v0.1+."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_os_release_handles_quoted_and_unquoted_values() {
        let body =
            "NAME=\"Ubuntu\"\nVERSION_ID=24.04\nID=ubuntu\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n";
        #[cfg(target_os = "linux")]
        {
            assert_eq!(parse_os_release_field(body, "ID"), Some("ubuntu".into()));
            assert_eq!(
                parse_os_release_field(body, "PRETTY_NAME"),
                Some("Ubuntu 24.04.1 LTS".into())
            );
            assert_eq!(
                parse_os_release_field(body, "VERSION_ID"),
                Some("24.04".into())
            );
        }
        let _ = body;
    }

    #[test]
    fn detect_returns_an_info_struct_with_arch() {
        let info = detect();
        assert!(!info.arch.is_empty());
    }
}
