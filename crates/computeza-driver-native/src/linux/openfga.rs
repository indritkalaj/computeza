//! OpenFGA fine-grained authorization. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, CliSymlink, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const UNIT_NAME: &str = "computeza-openfga.service";
pub const DEFAULT_HTTP_PORT: u16 = 8080;

// Verified May 2026 against the GitHub Releases API.
const OPENFGA_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.15.1",
        url: "https://github.com/openfga/openfga/releases/download/v1.15.1/openfga_1.15.1_linux_amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "1.15.0",
        url: "https://github.com/openfga/openfga/releases/download/v1.15.0/openfga_1.15.0_linux_amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    OPENFGA_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/openfga"),
            port: DEFAULT_HTTP_PORT,
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
        "run".into(),
        "--datastore-engine".into(),
        "memory".into(),
        "--http-addr".into(),
        format!("127.0.0.1:{}", opts.port),
        "--grpc-addr".into(),
        format!("127.0.0.1:{}", opts.port + 1),
    ];
    service::install_service(
        ServiceInstall {
            component: "openfga",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "openfga",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: None,
            cli_symlink: Some(CliSymlink {
                short_name: "openfga",
                binary_name: "openfga",
            }),
            env: Vec::new(),
            exec_start_pre_args: Vec::new(),
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
            root_dir: PathBuf::from("/var/lib/computeza/openfga"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("openfga", &opts.root_dir, &opts.unit_name, Some("openfga")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/openfga");
    if !tokio::fs::try_exists(root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-openfga".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_HTTP_PORT),
        data_dir: None,
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => OPENFGA_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&OPENFGA_BUNDLES[0]),
        None => &OPENFGA_BUNDLES[0],
    }
}
