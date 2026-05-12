//! Lakekeeper Iceberg REST catalog. Linux install path.
//!
//! Note: Lakekeeper needs a PostgreSQL backing store. v0.0.x assumes
//! one is already running on `127.0.0.1:5432` -- install postgres first.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-lakekeeper.service";
pub const DEFAULT_PORT: u16 = 8181;

const LAKEKEEPER_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "0.9.0",
        // TODO: verify against https://github.com/lakekeeper/lakekeeper/releases
        url: "https://github.com/lakekeeper/lakekeeper/releases/download/v0.9.0/lakekeeper-x86_64-unknown-linux-musl.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "0.8.0",
        url: "https://github.com/lakekeeper/lakekeeper/releases/download/v0.8.0/lakekeeper-x86_64-unknown-linux-musl.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    LAKEKEEPER_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/lakekeeper"),
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
    let args = vec!["serve".into()];
    service::install_service(
        ServiceInstall {
            component: "lakekeeper",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "lakekeeper",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: None,
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
            root_dir: PathBuf::from("/var/lib/computeza/lakekeeper"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("lakekeeper", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/lakekeeper");
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-lakekeeper".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: None,
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => LAKEKEEPER_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&LAKEKEEPER_BUNDLES[0]),
        None => &LAKEKEEPER_BUNDLES[0],
    }
}
