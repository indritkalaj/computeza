//! Apache XTable Iceberg<->Delta<->Hudi metadata sync. Linux install path.
//!
//! XTable is a Java sidecar. v0.0.x installs require a host JRE >=17
//! to be already present; bundling a JRE alongside the install is a
//! v0.1 task. We download the runner JAR and register a systemd unit
//! that shells out to `java -jar ...`.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-xtable.service";
pub const DEFAULT_PORT: u16 = 8090;

const XTABLE_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "0.2.0",
        // TODO: verify against https://github.com/apache/incubator-xtable/releases
        url: "https://github.com/apache/incubator-xtable/releases/download/v0.2.0-incubating/xtable-runner-0.2.0.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
    Bundle {
        version: "0.1.0",
        url: "https://github.com/apache/incubator-xtable/releases/download/v0.1.0-incubating/xtable-runner-0.1.0.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    XTABLE_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/xtable"),
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
    // XTable is a Java app; we shell to `java -jar` instead of a
    // native binary. The bundle is expected to contain
    // `xtable-runner.jar` at the root.
    let args = vec!["-jar".into(), "xtable-runner.jar".into()];
    service::install_service(
        ServiceInstall {
            component: "xtable",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "java", // expects $PATH; system JRE
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
            root_dir: PathBuf::from("/var/lib/computeza/xtable"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("xtable", &opts.root_dir, &opts.unit_name, None).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/xtable");
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-xtable".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
        data_dir: None,
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => XTABLE_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&XTABLE_BUNDLES[0]),
        None => &XTABLE_BUNDLES[0],
    }
}
