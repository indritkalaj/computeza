//! Kanidm install path on Linux. Built on `linux::service`.
//!
//! v0.0.x ships the kanidmd server binary from the upstream GitHub
//! release tarball, writes a minimal `server.toml`, and registers a
//! `computeza-kanidm.service` systemd unit. Production deployments
//! need real TLS certs and a tightened HBA -- this default is dev-
//! grade self-signed and binds to loopback only.
//!
//! Vendor URLs marked TODO until the next release-pinning pass.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, CliSymlink, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const SERVICE_NAME: &str = "computeza-kanidm";
pub const UNIT_NAME: &str = "computeza-kanidm.service";
pub const DEFAULT_PORT: u16 = 8443;

// IMPORTANT: kanidm does NOT publish prebuilt binaries on GitHub
// releases (the asset list is empty across all v1.x tags). The
// upstream-supported install paths are distro package managers
// (zypper / dnf / apt / pacman / apk / pkg), `cargo install`, or
// Docker. Our download-tarball-from-GitHub assumption was wrong.
//
// Until the package-manager dispatch lands (v0.1 task), the Bundle
// URLs below remain pinned at the current stable versions but they
// will 404. Install attempts surface that 404 verbatim through the
// existing fetch error chain so the operator sees what went wrong.
//
// Tracking the actual install strategy refactor as the next pass
// over this module.
const KANIDM_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.10.1",
        // No corresponding asset on the GitHub release page;
        // see module-level note above.
        url: "https://github.com/kanidm/kanidm/releases/download/v1.10.1/kanidm-1.10.1-linux-x86_64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "kanidm-1.10.1-linux-x86_64",
    },
    Bundle {
        version: "1.9.3",
        url: "https://github.com/kanidm/kanidm/releases/download/v1.9.3/kanidm-1.9.3-linux-x86_64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "kanidm-1.9.3-linux-x86_64",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    KANIDM_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/kanidm"),
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
    let config = ConfigFile {
        filename: "server.toml".into(),
        contents: format!(
            "bindaddress = \"127.0.0.1:{port}\"\n\
             domain = \"localhost\"\n\
             origin = \"https://localhost:{port}\"\n\
             db_path = \"{root}/data/kanidm.db\"\n\
             role = \"WriteReplica\"\n",
            port = opts.port,
            root = opts.root_dir.display(),
        ),
    };
    let args = vec![
        "server".into(),
        "--config".into(),
        opts.root_dir
            .join("server.toml")
            .to_string_lossy()
            .into_owned(),
    ];
    let install = ServiceInstall {
        component: "kanidm",
        root_dir: opts.root_dir,
        bundle,
        binary_name: "kanidmd",
        args,
        port: opts.port,
        unit_name: opts.unit_name,
        config: Some(config),
        cli_symlink: Some(CliSymlink {
            short_name: "kanidm",
            binary_name: "kanidm",
        }),
    };
    service::install_service(install, progress).await
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub unit_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/kanidm"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("kanidm", &opts.root_dir, &opts.unit_name, Some("kanidm")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    detect_under("/var/lib/computeza/kanidm", "kanidm").await
}

/// Convenience that probes one root for a `data/` dir plus a binaries
/// cache populated by `service::install_service`.
async fn detect_under(root: &str, component: &str) -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from(root);
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: format!("computeza-{component}"),
        owner: "computeza".into(),
        version: None,
        port: None,
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => KANIDM_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&KANIDM_BUNDLES[0]),
        None => &KANIDM_BUNDLES[0],
    }
}
