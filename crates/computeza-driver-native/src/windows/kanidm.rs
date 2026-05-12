#![allow(missing_docs)]
//! Kanidm install on Windows -- not currently supported by upstream.
//!
//! Kanidm's release process ships Linux + macOS binaries only. There
//! is no `kanidmd.exe` published. We still expose the same
//! `install` / `uninstall` / `detect_installed` surface as the other
//! OSes so the UI dispatcher stays uniform; the install path
//! returns a clear error explaining the upstream limitation.

use std::path::PathBuf;

use thiserror::Error;

use crate::progress::ProgressHandle;

pub const DEFAULT_PORT: u16 = 8443;

/// Windows-specific kanidm install errors. The only variant today is
/// "upstream doesn't ship a Windows binary"; if/when that changes the
/// shape grows like `postgres::InstallError`.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error(
        "Kanidm does not currently publish a Windows native binary. \
         Upstream supports Linux and macOS only; track \
         https://github.com/kanidm/kanidm/issues for Windows progress. \
         Workaround: run kanidm on a Linux host (WSL2 or a separate \
         VM) and point the reconciler at it from this Computeza \
         instance."
    )]
    UpstreamUnsupported,
}

/// Configuration carried so the UI dispatch table can pretend kanidm
/// is configurable on every OS. None of these fields are actually
/// honoured -- install returns `UpstreamUnsupported` regardless.
#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub root_dir: PathBuf,
    pub port: u16,
    pub service_name: String,
    pub version: Option<String>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        let programdata =
            std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from("C:\\ProgramData"));
        Self {
            root_dir: PathBuf::from(programdata).join("Computeza").join("kanidm"),
            port: DEFAULT_PORT,
            service_name: "computeza-kanidm".into(),
            version: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Installed;

pub async fn install_with_progress(
    _opts: InstallOptions,
    _progress: &ProgressHandle,
) -> Result<Installed, InstallError> {
    Err(InstallError::UpstreamUnsupported)
}

pub async fn install(opts: InstallOptions) -> Result<Installed, InstallError> {
    install_with_progress(opts, &ProgressHandle::noop()).await
}

#[derive(Clone, Debug)]
pub struct UninstallOptions {
    pub root_dir: PathBuf,
    pub service_name: String,
}

impl Default for UninstallOptions {
    fn default() -> Self {
        let programdata =
            std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from("C:\\ProgramData"));
        Self {
            root_dir: PathBuf::from(programdata).join("Computeza").join("kanidm"),
            service_name: "computeza-kanidm".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Uninstalled {
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

pub async fn uninstall(_opts: UninstallOptions) -> Result<Uninstalled, InstallError> {
    // Symmetric to install: no Windows binary, nothing to tear down.
    // Returning Ok with a single explanatory step keeps the UI's
    // result page useful (instead of "service does not exist" warnings).
    Ok(Uninstalled {
        steps: vec!["kanidm is not installed on Windows (upstream does not ship a Windows binary)".into()],
        warnings: vec![],
    })
}

/// Detection on Windows: kanidm cannot be installed, so by definition
/// there's nothing to detect.
pub async fn detect_installed() -> Vec<crate::detect::DetectedInstall> {
    Vec::new()
}

pub fn available_versions() -> &'static [crate::fetch::Bundle] {
    &[]
}
