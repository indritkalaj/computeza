//! Detection of already-installed components on the host.
//!
//! The install wizard calls into this module on every GET /install to
//! discover what's already on the system, then surfaces a "Detected
//! installs" panel and pre-fills the form with non-colliding defaults
//! (port, data dir, service name).
//!
//! Detection is conservative: we only report installs we are highly
//! confident exist. False positives are worse than false negatives
//! because they push the operator toward unnecessary port shuffles.
//! False negatives at worst result in a "service already exists"
//! error from the install path, which the operator can then resolve
//! by re-entering values manually.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One detected PostgreSQL install on the host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DetectedInstall {
    /// Stable identifier for display + form pre-fill (typically the
    /// service / unit / launchd label).
    pub identifier: String,
    /// Who owns this install. Distinguishes Computeza-managed
    /// instances ("computeza") from packages installed via the host
    /// package manager ("EDB", "Homebrew", "apt", ...).
    pub owner: String,
    /// Major version string if known (e.g. "18", "17").
    pub version: Option<String>,
    /// Port the daemon is configured to listen on, if known.
    pub port: Option<u16>,
    /// Data directory if known.
    pub data_dir: Option<PathBuf>,
    /// Binary directory if known.
    pub bin_dir: Option<PathBuf>,
}

impl DetectedInstall {
    /// Human-readable summary line for the wizard's "Detected" panel.
    pub fn summary(&self) -> String {
        let mut out = format!("{} ({})", self.identifier, self.owner);
        if let Some(v) = &self.version {
            out.push_str(&format!(", PostgreSQL {v}"));
        }
        if let Some(p) = self.port {
            out.push_str(&format!(", port {p}"));
        }
        if let Some(d) = &self.data_dir {
            out.push_str(&format!(", data {}", d.display()));
        }
        out
    }
}

/// Dispatch to the per-OS detect implementation. Returns an empty
/// list on unsupported platforms.
pub async fn postgres() -> Vec<DetectedInstall> {
    #[cfg(target_os = "windows")]
    {
        crate::windows::postgres::detect_installed().await
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::postgres::detect_installed().await
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos::postgres::detect_installed().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Compute non-colliding defaults for a new install given what's
/// already on the host. The wizard renders the suggested values as
/// HTML `placeholder` attributes -- the operator can accept them with
/// one tab or type their own.
///
/// Rules:
/// - Port: the lowest port at or above 5432 that no detected install
///   already claims.
/// - Service name suffix: when any of the detected installs is
///   computeza-managed, suggest `computeza-postgres-<major>` to
///   avoid colliding with the existing service. Otherwise the
///   default `computeza-postgres` is fine.
/// - Data dir suffix: same logic -- when colliding, suggest a
///   version-suffixed directory.
pub fn smart_defaults(
    detected: &[DetectedInstall],
    requested_version_major: Option<&str>,
) -> SmartDefaults {
    let mut used_ports: Vec<u16> = detected.iter().filter_map(|d| d.port).collect();
    used_ports.sort_unstable();
    let mut port = 5432u16;
    for p in &used_ports {
        if *p == port {
            port = port.saturating_add(1);
        }
    }
    let has_computeza = detected
        .iter()
        .any(|d| d.owner.eq_ignore_ascii_case("computeza"));
    let suffix = match (has_computeza, requested_version_major) {
        (true, Some(v)) => Some(v.to_string()),
        (true, None) => Some(format!("alt-{}", detected.len())),
        (false, _) => None,
    };
    SmartDefaults { port, suffix }
}

/// Output of [`smart_defaults`].
#[derive(Clone, Debug)]
pub struct SmartDefaults {
    /// Suggested TCP port (placeholder in the wizard's port field).
    pub port: u16,
    /// Suggested suffix for service-name and data-dir to avoid
    /// colliding with an existing Computeza-managed install.
    /// `None` means the unsuffixed defaults are fine.
    pub suffix: Option<String>,
}

impl SmartDefaults {
    /// Suggested service name. `computeza-postgres` if no suffix,
    /// `computeza-postgres-<suffix>` otherwise.
    pub fn service_name(&self) -> String {
        match &self.suffix {
            Some(s) => format!("computeza-postgres-{s}"),
            None => "computeza-postgres".into(),
        }
    }

    /// Suggested data dir leaf (under the platform's root). Returns
    /// just the leaf name; the wizard renders the full path with the
    /// per-OS prefix.
    pub fn data_dir_leaf(&self) -> String {
        match &self.suffix {
            Some(s) => format!("postgres-{s}"),
            None => "postgres".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_defaults_first_install_uses_canonical_values() {
        let d = smart_defaults(&[], None);
        assert_eq!(d.port, 5432);
        assert_eq!(d.service_name(), "computeza-postgres");
        assert_eq!(d.data_dir_leaf(), "postgres");
    }

    #[test]
    fn smart_defaults_avoids_used_port() {
        let detected = vec![DetectedInstall {
            identifier: "computeza-postgres".into(),
            owner: "computeza".into(),
            version: Some("18".into()),
            port: Some(5432),
            data_dir: None,
            bin_dir: None,
        }];
        let d = smart_defaults(&detected, Some("17"));
        assert_eq!(d.port, 5433);
        assert_eq!(d.service_name(), "computeza-postgres-17");
        assert_eq!(d.data_dir_leaf(), "postgres-17");
    }

    #[test]
    fn smart_defaults_skips_through_a_dense_port_range() {
        let detected: Vec<DetectedInstall> = [5432, 5433, 5434]
            .into_iter()
            .map(|p| DetectedInstall {
                identifier: format!("computeza-postgres-{p}"),
                owner: "computeza".into(),
                version: None,
                port: Some(p),
                data_dir: None,
                bin_dir: None,
            })
            .collect();
        let d = smart_defaults(&detected, None);
        assert_eq!(d.port, 5435);
    }

    #[test]
    fn host_managed_installs_do_not_force_a_suffix() {
        let detected = vec![DetectedInstall {
            identifier: "PostgreSQL 17".into(),
            owner: "EDB".into(),
            version: Some("17".into()),
            port: Some(5432),
            data_dir: None,
            bin_dir: None,
        }];
        let d = smart_defaults(&detected, Some("18"));
        // Port still shifts because 5432 is taken...
        assert_eq!(d.port, 5433);
        // ...but the service-name suffix does not fire (no Computeza
        // service to collide with).
        assert_eq!(d.service_name(), "computeza-postgres");
    }
}
