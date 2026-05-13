//! GreptimeDB unified observability database. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-greptime.service";
pub const DEFAULT_HTTP_PORT: u16 = 4000;

// Verified May 2026 against the GitHub Releases API.
const GREPTIME_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.0.1",
        url: "https://github.com/GreptimeTeam/greptimedb/releases/download/v1.0.1/greptime-linux-amd64-v1.0.1.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "1.0.0",
        url: "https://github.com/GreptimeTeam/greptimedb/releases/download/v1.0.0/greptime-linux-amd64-v1.0.0.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    GREPTIME_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/greptime"),
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
        "standalone".into(),
        "start".into(),
        "--http-addr".into(),
        format!("127.0.0.1:{}", opts.port),
        "--data-home".into(),
        opts.root_dir.join("data").to_string_lossy().into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "greptime",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "greptime",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: None,
            cli_symlink: None,
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
            root_dir: PathBuf::from("/var/lib/computeza/greptime"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("greptime", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/greptime");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-greptime".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_HTTP_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => GREPTIME_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&GREPTIME_BUNDLES[0]),
        None => &GREPTIME_BUNDLES[0],
    }
}
