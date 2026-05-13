//! Databend columnar SQL engine. Linux install path.
//!
//! Databend is a two-process system: `databend-meta` (Raft-based
//! metadata store) and `databend-query` (SQL engine that talks to
//! meta). The query refuses to start without explicit meta
//! endpoints -- there's no embedded-meta mode in current releases.
//!
//! The driver installs BOTH as sibling systemd units:
//!
//! 1. `computeza-databend-meta.service` -- single-node Raft meta,
//!    binds the gRPC API on 127.0.0.1:9191. Comes up first.
//! 2. `computeza-databend.service` (query) -- binds the HTTP
//!    handler on 127.0.0.1:8000 (operator-configurable port),
//!    points at the local meta via `[meta] endpoints =
//!    ["127.0.0.1:9191"]`. Comes up second.
//!
//! Both share the binary cache under `<root>/binaries/<version>/`
//! (one tarball, two binaries: `databend-meta` + `databend-query`).
//! Operators uninstall via `/install -> databend -> Uninstall`,
//! which tears down both units.

use std::path::PathBuf;

use crate::{
    fetch::{ArchiveKind, Bundle},
    progress::ProgressHandle,
};

use super::service::{
    self, ConfigFile, InstalledService, ServiceError, ServiceInstall, Uninstalled,
};

pub const UNIT_NAME: &str = "computeza-databend.service";
pub const META_UNIT_NAME: &str = "computeza-databend-meta.service";
pub const DEFAULT_PORT: u16 = 8000;
/// gRPC port the meta service binds. Hard-coded for v0.0.x; the
/// query config below references the same value.
const META_GRPC_PORT: u16 = 9191;
/// Raft inter-node port. Single-node deployments don't actually
/// use this for traffic, but databend-meta requires it to be set.
const META_RAFT_PORT: u16 = 9192;

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

    // Step 1: databend-meta must come up first; databend-query
    // will fail to start until the meta service is listening on
    // its gRPC port.
    let meta_config = ConfigFile {
        filename: "databend-meta.toml".into(),
        contents: format!(
            "log_dir = \"{root}/meta/logs\"\n\
             admin_api_address = \"127.0.0.1:{admin}\"\n\
             grpc_api_address = \"127.0.0.1:{grpc}\"\n\
             grpc_api_advertise_host = \"127.0.0.1\"\n\
             \n\
             [raft_config]\n\
             id = 1\n\
             raft_listen_host = \"127.0.0.1\"\n\
             raft_advertise_host = \"127.0.0.1\"\n\
             raft_api_port = {raft}\n\
             raft_dir = \"{root}/meta/raft\"\n\
             single = true\n",
            root = opts.root_dir.display(),
            admin = META_GRPC_PORT + 10, // 9201; sidecar admin port
            grpc = META_GRPC_PORT,
            raft = META_RAFT_PORT,
        ),
        // Same reasoning as the query config below: v0.0.x's
        // template is still evolving (raft ports, advertise-host
        // semantics, etc.), so re-installs overwrite to roll out
        // the latest fixes. Operator overrides land in
        // `databend-meta.local.toml` once v0.1 ships drop-ins.
        overwrite_if_present: true,
    };
    let meta_args = vec![
        "-c".into(),
        opts.root_dir
            .join("databend-meta.toml")
            .to_string_lossy()
            .into_owned(),
    ];
    service::install_service(
        ServiceInstall {
            component: "databend-meta",
            root_dir: opts.root_dir.clone(),
            bundle: bundle.clone(),
            binary_name: "databend-meta",
            args: meta_args,
            port: META_GRPC_PORT,
            unit_name: META_UNIT_NAME.into(),
            config: Some(meta_config),
            cli_symlink: None,
            env: Vec::new(),
            exec_start_pre_args: Vec::new(),
        },
        progress,
    )
    .await?;

    // Step 2: databend-query, pointing at the just-started meta.
    // databend's config template is operator-extensible (operators
    // commonly add `[[catalog]]` blocks pointing at lakekeeper, or
    // swap storage backends to S3 via garage). overwrite_if_present
    // is false so re-installs preserve those edits.
    // Databend's query process exposes FIVE listening endpoints in
    // addition to the HTTP handler the operator sees:
    //   - flight_api  (gRPC; default 8080 -- collides with OpenFGA)
    //   - admin_api   (default 8080 also)
    //   - metric_api  (Prometheus; default 7070)
    //   - mysql_handler (default 3307)
    //   - clickhouse_http_handler (default 8124)
    //
    // Of those, only flight_api + admin_api hit conflicts on our
    // standard port plan (OpenFGA owns 8080). We pin all the
    // commonly-conflicting ones to offsets of opts.port so a
    // re-run with `--port 9100` shifts the whole cluster
    // together. Operators wanting to expose the mysql /
    // clickhouse-compatible handlers add their own port lines
    // -- those default OFF if not configured.
    let flight_port = opts.port + 1; // 8001
    let admin_port = opts.port + 2; // 8002
    let metric_port = opts.port + 3; // 8003
    let query_config = ConfigFile {
        filename: "databend-query.toml".into(),
        contents: format!(
            "[query]\n\
             http_handler_host = \"127.0.0.1\"\n\
             http_handler_port = {port}\n\
             flight_api_address = \"127.0.0.1:{flight_port}\"\n\
             admin_api_address = \"127.0.0.1:{admin_port}\"\n\
             metric_api_address = \"127.0.0.1:{metric_port}\"\n\
             tenant_id = \"computeza\"\n\
             cluster_id = \"local\"\n\
             \n\
             # v0.0.x: define a `root` user with no_password auth,
             # accessible only from 127.0.0.1. Required because\n\
             # Databend's HTTP query handler ships with NO built-in\n\
             # users -- the `[meta]` block below configures meta-service\n\
             # auth, not the query-user database. Without an explicit\n\
             # `[[query.users]]` entry, every HTTP basic-auth attempt\n\
             # 401s with `User 'root'@'%' does not exist.`\n\
             #\n\
             # `no_password` is a v0.0.x trade-off:\n\
             #   - localhost-only (hostname = 127.0.0.1) so external\n\
             #     callers can't authenticate without their own\n\
             #     `[[query.users]]` block + sha256 hash.\n\
             #   - The operator console runs on the same host as\n\
             #     Databend in v0.0.x, so localhost is the only\n\
             #     legitimate caller anyway.\n\
             # v0.1 switches to `auth_type = sha256_password` with a\n\
             # vault-stored secret (see AGENTS.md \"Deferred work\").\n\
             [[query.users]]\n\
             name = \"root\"\n\
             hostname = \"127.0.0.1\"\n\
             auth_type = \"no_password\"\n\
             \n\
             [meta]\n\
             endpoints = [\"127.0.0.1:{meta_grpc}\"]\n\
             username = \"root\"\n\
             password = \"root\"\n\
             client_timeout_in_second = 60\n\
             auto_sync_interval = 60\n\
             \n\
             [storage]\n\
             type = \"fs\"\n\
             [storage.fs]\n\
             data_path = \"{root}/data\"\n",
            port = opts.port,
            flight_port = flight_port,
            admin_port = admin_port,
            metric_port = metric_port,
            meta_grpc = META_GRPC_PORT,
            root = opts.root_dir.display(),
        ),
        // Was `false` (preserve operator edits) but flipped to
        // `true` because v0.0.x's config-template is still
        // evolving -- e.g. the [meta] block was just added in
        // ef0627f, and operators stuck with a pre-ef0627f config
        // can't get the meta wiring without a manual edit. v0.1
        // separates driver-owned base config from operator
        // overrides via a `databend-query.local.toml` drop-in
        // that does NOT get overwritten; until then the
        // re-install authoritatively re-renders the base. Lost
        // edits are surfaced in the install-result page so
        // operators can re-apply them.
        overwrite_if_present: true,
    };
    let query_args = vec![
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
            args: query_args,
            port: opts.port,
            unit_name: opts.unit_name,
            config: Some(query_config),
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
            root_dir: PathBuf::from("/var/lib/computeza/databend"),
            unit_name: UNIT_NAME.into(),
        }
    }
}

pub async fn uninstall(opts: UninstallOptions) -> Result<Uninstalled, ServiceError> {
    // Tear down the meta service first (best-effort) so the
    // following uninstall_service call cleans up cleanly. We don't
    // surface meta-side warnings to the operator -- they're not
    // actionable.
    let _ = service::uninstall_service("databend-meta", &opts.root_dir, META_UNIT_NAME, None).await;
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
