//! Restate durable execution. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-restate.service";
pub const DEFAULT_INGRESS_PORT: u16 = 8080;

// Verified May 2026 against the GitHub Releases API. Restate ships
// three separate tarballs per release (restate-server, restate-cli,
// restatectl); the driver installs only the server. The CLI tools
// can be fetched separately by operators who need them.
//
// Format is .tar.xz -- handled by ArchiveKind::TarXz via liblzma
// (statically linked, so virgin Linux hosts without xz-utils still
// work).
const RESTATE_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.6.2",
        url: "https://github.com/restatedev/restate/releases/download/v1.6.2/restate-server-x86_64-unknown-linux-musl.tar.xz",
        kind: ArchiveKind::TarXz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "1.6.1",
        url: "https://github.com/restatedev/restate/releases/download/v1.6.1/restate-server-x86_64-unknown-linux-musl.tar.xz",
        kind: ArchiveKind::TarXz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "1.6.0",
        url: "https://github.com/restatedev/restate/releases/download/v1.6.0/restate-server-x86_64-unknown-linux-musl.tar.xz",
        kind: ArchiveKind::TarXz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    RESTATE_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/restate"),
            port: DEFAULT_INGRESS_PORT,
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
    let args = vec![
        "--node-name".into(),
        "computeza".into(),
        "--base-dir".into(),
        opts.root_dir.join("data").to_string_lossy().into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "restate",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "restate-server",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: None,
            // restatectl ships in a separate tarball -- not bundled.
            cli_symlink: None,
        },
        progress,
    )
    .await
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub unit_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/var/lib/computeza/restate"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("restate", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/restate");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-restate".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_INGRESS_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => RESTATE_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&RESTATE_BUNDLES[0]),
        None => &RESTATE_BUNDLES[0],
    }
}
