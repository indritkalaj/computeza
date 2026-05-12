//! Garage S3-compatible object storage. Linux install path.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, CliSymlink, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const UNIT_NAME: &str = "computeza-garage.service";
pub const DEFAULT_S3_PORT: u16 = 3900;

// Verified May 2026 -- the deuxfleurs CDN serves these URLs.
const GARAGE_BUNDLES: &[Bundle] = &[
    Bundle {
        version: "2.0.0",
        url: "https://garagehq.deuxfleurs.fr/_releases/v2.0.0/x86_64-unknown-linux-musl/garage",
        kind: ArchiveKind::Raw,
        sha256: None,
        bin_subpath: "bin",
    },
    Bundle {
        version: "1.1.0",
        url: "https://garagehq.deuxfleurs.fr/_releases/v1.1.0/x86_64-unknown-linux-musl/garage",
        kind: ArchiveKind::Raw,
        sha256: None,
        bin_subpath: "bin",
    },
];

pub fn available_versions() -> &'static [Bundle] {
    GARAGE_BUNDLES
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
            root_dir: PathBuf::from("/var/lib/computeza/garage"),
            port: DEFAULT_S3_PORT,
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
    // Garage binds four ports. The wizard collects the S3 API port
    // and we derive the others by adding small offsets so re-runs
    // with `port = 4000` don't collide with the canonical 3900-3903
    // range either.
    let s3_port = opts.port;
    let rpc_port = s3_port + 1;
    let web_port = s3_port + 2;
    let admin_port = s3_port + 3;
    let config = ConfigFile {
        filename: "garage.toml".into(),
        contents: format!(
            "metadata_dir = \"{root}/data/meta\"\n\
             data_dir = \"{root}/data/data\"\n\
             db_engine = \"sqlite\"\n\
             replication_factor = 1\n\
             rpc_bind_addr = \"127.0.0.1:{rpc}\"\n\
             rpc_public_addr = \"127.0.0.1:{rpc}\"\n\
             rpc_secret = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
             \n\
             [s3_api]\n\
             api_bind_addr = \"127.0.0.1:{s3}\"\n\
             s3_region = \"garage\"\n\
             root_domain = \".s3.garage.local\"\n\
             \n\
             [s3_web]\n\
             bind_addr = \"127.0.0.1:{web}\"\n\
             root_domain = \".web.garage.local\"\n\
             index = \"index.html\"\n\
             \n\
             [admin]\n\
             api_bind_addr = \"127.0.0.1:{admin}\"\n\
             admin_token = \"change-me\"\n\
             metrics_token = \"change-me\"\n",
            root = opts.root_dir.display(),
            s3 = s3_port,
            rpc = rpc_port,
            web = web_port,
            admin = admin_port,
        ),
    };
    let args = vec![
        "-c".into(),
        opts.root_dir
            .join("garage.toml")
            .to_string_lossy()
            .into_owned(),
        "server".into(),
    ];
    service::install_service(
        ServiceInstall {
            component: "garage",
            root_dir: opts.root_dir,
            bundle,
            binary_name: "garage",
            args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: Some(config),
            cli_symlink: Some(CliSymlink {
                short_name: "garage",
                binary_name: "garage",
            }),
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
            root_dir: PathBuf::from("/var/lib/computeza/garage"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    service::uninstall_service("garage", &opts.root_dir, &opts.unit_name, Some("garage")).await
}

pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    let root = PathBuf::from("/var/lib/computeza/garage");
    if !tokio::fs::try_exists(root.join("data"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![crate::detect::DetectedInstall {
        identifier: "computeza-garage".into(),
        owner: "computeza".into(),
        version: None,
        port: Some(DEFAULT_S3_PORT),
        data_dir: Some(root.join("data")),
        bin_dir: None,
    }]
}

fn pick_bundle(requested: Option<&str>) -> &'static Bundle {
    match requested {
        Some(v) => GARAGE_BUNDLES
            .iter()
            .find(|b| b.version == v)
            .unwrap_or(&GARAGE_BUNDLES[0]),
        None => &GARAGE_BUNDLES[0],
    }
}
