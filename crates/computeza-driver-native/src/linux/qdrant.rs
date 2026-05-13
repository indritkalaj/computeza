//! Qdrant vector store. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const UNIT_NAME: &str = "computeza-qdrant.service";
pub const DEFAULT_HTTP_PORT: u16 = 6333;

// Verified May 2026 against the GitHub Releases API.
const QDRANT_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.18.0",
        url: "https://github.com/qdrant/qdrant/releases/download/v1.18.0/qdrant-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "1.17.1",
        url: "https://github.com/qdrant/qdrant/releases/download/v1.17.1/qdrant-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    QDRANT_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/qdrant"),
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
    let config = ConfigFile {
        filename: "config.yaml".into(),
        contents: format!(
            "service:\n  http_port: {port}\n  grpc_port: {grpc}\n  host: 127.0.0.1\nstorage:\n  storage_path: {root}/data\n",
            port = opts.port,
            grpc = opts.port + 1,
            root = opts.root_dir.display(),
        ),
    };
    let args = vec![
        "--config-path".into(),
        opts.root_dir
            .join("config.yaml")
            .to_string_lossy()
            .into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "qdrant",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "qdrant",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: Some(config),
            cli_symlink: None,
            env: Vec::new(),
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
            root_dir: PathBuf::from("/var/lib/computeza/qdrant"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("qdrant", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/qdrant");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-qdrant".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_HTTP_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => QDRANT_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&QDRANT_BUNDLES[0]),
        None => &QDRANT_BUNDLES[0],
    }
}
