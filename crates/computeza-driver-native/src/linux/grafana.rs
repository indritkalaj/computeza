//! Grafana visualisation. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{self, InstalledService, ServiceError, ServiceInstall, Uninstalled};

pub const UNIT_NAME: &str = "computeza-grafana.service";
pub const DEFAULT_PORT: u16 = 3000;

// Verified May 2026. Grafana doesn't publish on GitHub releases;
// it has its own CDN at dl.grafana.com. The archive root directory
// is `grafana-vX.Y.Z/` with `bin/`, `conf/`, etc. inside.
const GRAFANA_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "13.0.1",
        url: "https://dl.grafana.com/oss/release/grafana-13.0.1.linux-amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "grafana-v13.0.1/bin",
    },
    Bundle {
        version: "12.4.3",
        url: "https://dl.grafana.com/oss/release/grafana-12.4.3.linux-amd64.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "grafana-v12.4.3/bin",
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

    // Grafana's `--homepath` must point at the EXTRACTED tarball
    // root (the directory that contains `conf/defaults.ini`,
    // `public/`, `plugins-bundled/`), not at an arbitrary path
    // under `<root>/`. The shared `install_service` extracts into
    // `<root>/binaries/<version>/<bin_subpath_parent>/`. We
    // reconstruct that path here from the bundle metadata.
    let version_subdir = std::path::Path::new(bundle.bin_subpath)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let homepath = opts
        .root_dir
        .join("binaries")
        .join(bundle.version)
        .join(&version_subdir);

    // Pin data + logs to stable paths under <root> so grafana's
    // SQLite DB survives version bumps (the binaries/ cache may
    // be pruned in v0.1+ but data/ + logs/ must persist). The
    // env vars override grafana.ini's `[paths]` section at
    // runtime.
    let data_path = opts.root_dir.join("data");
    let logs_path = opts.root_dir.join("logs");
    let plugins_path = opts.root_dir.join("plugins");
    let provisioning_path = opts.root_dir.join("conf").join("provisioning");

    // Pre-create the data / logs / plugins / provisioning
    // directories so grafana doesn't have to (and so a
    // misconfigured sandbox fails-fast with a clearer error than
    // grafana's own).
    for p in [&data_path, &logs_path, &plugins_path, &provisioning_path] {
        if let Err(e) = tokio::fs::create_dir_all(p).await {
            tracing::warn!(path = %p.display(), error = %e, "pre-creating grafana state dir failed; grafana will retry");
        }
    }

    let env = vec![
        (
            "GF_PATHS_DATA".into(),
            data_path.to_string_lossy().into_owned(),
        ),
        (
            "GF_PATHS_LOGS".into(),
            logs_path.to_string_lossy().into_owned(),
        ),
        (
            "GF_PATHS_PLUGINS".into(),
            plugins_path.to_string_lossy().into_owned(),
        ),
        (
            "GF_PATHS_PROVISIONING".into(),
            provisioning_path.to_string_lossy().into_owned(),
        ),
    ];

    let args = vec![
        "server".into(),
        "--homepath".into(),
        homepath.to_string_lossy().into_owned(),
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
            env,
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
