//! Databend columnar SQL engine. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const UNIT_NAME: &str = "computeza-databend.service";
pub const DEFAULT_PORT: u16 = 8000;

// Repo moved from datafuselabs to databendlabs. Versions use the
// `patch` suffix on stable; nightly tags also exist on the same repo
// but we pin to patch releases.
const DATABEND_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.2.888-patch-8",
        url: "https://github.com/databendlabs/databend/releases/download/v1.2.888-patch-8/databend-v1.2.888-patch-8-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "bin",
    },
    Bundle {
        version: "1.2.880-patch-1",
        url: "https://github.com/databendlabs/databend/releases/download/v1.2.880-patch-1/databend-v1.2.880-patch-1-x86_64-unknown-linux-gnu.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "bin",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    DATABEND_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/databend"),
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
        filename: "databend-query.toml".into(),
        contents: format!(
            "[query]\nhttp_handler_host = \"127.0.0.1\"\nhttp_handler_port = {port}\n[storage]\ntype = \"fs\"\n[storage.fs]\ndata_path = \"{root}/data\"\n",
            port = opts.port,
            root = opts.root_dir.display(),
        ),
    };
    let args = vec![
        "-c".into(),
        opts.root_dir
            .join("databend-query.toml")
            .to_string_lossy()
            .into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "databend",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "databend-query",
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
            root_dir: PathBuf::from("/var/lib/computeza/databend"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("databend", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/databend");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-databend".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => DATABEND_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&DATABEND_BUNDLES[0]),
        None => &DATABEND_BUNDLES[0],
    }
}
