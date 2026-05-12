//! Kanidm install on macOS. Mirrors the Linux module shape, swapping
//! the Linux `service::install_service` for the launchd-based
//! `macos::service::install_service`.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, CliSymlink, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const SERVICE_LABEL: &str = "com.computeza.kanidm";
pub const DEFAULT_PORT: u16 = 8443;

const KANIDM_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "1.6.0",
        // TODO: verify against https://github.com/kanidm/kanidm/releases
        url: "https://github.com/kanidm/kanidm/releases/download/v1.6.0/kanidm-1.6.0-aarch64-apple-darwin.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "kanidm-1.6.0-aarch64-apple-darwin",
    },
    Bundle {
        version: "1.5.0",
        url: "https://github.com/kanidm/kanidm/releases/download/v1.5.0/kanidm-1.5.0-aarch64-apple-darwin.tar.gz",
        kind: ArchiveKind::TarGz,
        sha256: None,
        bin_subpath: "kanidm-1.5.0-aarch64-apple-darwin",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    KANIDM_BUNDLES
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub label: String,
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/Library/Application Support/Computeza/kanidm"),
            port: DEFAULT_PORT,
            label: SERVICE_LABEL.into(),
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
        contents: kanidm_server_toml(&opts.root_dir, opts.port),
    };
    let args = vec![
        "server".into(),
        "--config".into(),
        opts.root_dir
            .join("server.toml")
            .to_string_lossy()
            .into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "kanidm",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "kanidmd",
            args,
            port: opts.port,
            label: opts.label,
            config: Some(config),
            cli_symlink: Some(CliSymlink {
                short_name: "kanidm",
                binary_name: "kanidm",
            }),
        },
        progress,
    )
    .await
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub label: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/Library/Application Support/Computeza/kanidm"),
            label: SERVICE_LABEL.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("kanidm", &opts.root_dir, &opts.label, Some("kanidm")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/Library/Application Support/Computeza/kanidm");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-kanidm".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_PORT),
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

/// Minimal `server.toml`. Kanidm requires TLS even on loopback;
/// v0.0.x installs without a cert will fail to bind. The
/// `tls_chain` / `tls_key` paths are left unset; operators must
/// drop a cert + key there before the service can start.
pub(crate) fn kanidm_server_toml(root_dir: &std::path::Path, port: u16) -> String {
    format!(
        "bindaddress = \"127.0.0.1:{port}\"\n\
         domain = \"localhost\"\n\
         origin = \"https://localhost:{port}\"\n\
         db_path = \"{root}/data/kanidm.db\"\n\
         role = \"WriteReplica\"\n\
         # tls_chain = \"{root}/cert.pem\"  # required for production\n\
         # tls_key   = \"{root}/key.pem\"   # required for production\n",
        port = port,
        root = root_dir.display(),
    )
}
