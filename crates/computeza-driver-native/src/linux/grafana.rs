//! Grafana visualisation. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-grafana.service";
pub const DEFAULT_PORT: u16 = 3000;

const GRAFANA_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "11.4.0",
        // TODO: verify against https://grafana.com/grafana/download
        url: "https://dl.grafana.com/oss/release/grafana-11.4.0.linux-amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "grafana-v11.4.0/bin",
    },
    Bundle {
        version: "11.3.0",
        url: "https://dl.grafana.com/oss/release/grafana-11.3.0.linux-amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "grafana-v11.3.0/bin",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    GRAFANA_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/grafana"),
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
    let args = vec![
        "server".into(),
        "--homepath".into(),
        opts.root_dir.join("home").to_string_lossy().into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "grafana",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "grafana",
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
            root_dir: PathBuf::from("/var/lib/computeza/grafana"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("grafana", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/grafana");
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-grafana".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: None,
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => GRAFANA_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&GRAFANA_BUNDLES[0]),
        None => &GRAFANA_BUNDLES[0],
    }
}
