//! Linux-specific implementations.
//!
//! Compiled only on `target_os = "linux"`. systemd is assumed as the init
//! system; we don't pretend to support OpenRC / runit / sysvinit for v1.0.

// The linux driver modules are internal install plumbing -- pub items
// here exist so other modules in this crate (and tests) can call them,
// not because they're a documented external surface. Matches the
// existing precedent in `windows/kanidm.rs`. The crate-level
// `#![warn(missing_docs)]` still applies to all the cross-platform
// API surfaces (fetch::, detect::, os_detect::, progress::, ...).
#![allow(missing_docs)]

pub mod path;
pub mod postgres;
pub mod release;
pub mod service;
pub mod systemctl;

// ============================================================
// Post-install bootstrap convention (per v0.1 design doc §3.2)
// ============================================================
//
// Some managed components need post-daemon-start setup to reach
// steady state -- Garage needs a cluster layout applied, Lakekeeper
// needs a default project + warehouse created, Kanidm needs a
// reconciler service account minted, etc. The convention used here:
//
//   - The component's module exposes an async free function
//     `post_install_bootstrap(...) -> Result<Vec<BootstrapArtifact>, BootstrapError>`.
//   - The unified install orchestrator (ui-server::finalize_managed_install)
//     calls it AFTER `install_service` has returned and the daemon is
//     accepting connections.
//   - Each returned artifact is persisted by the orchestrator into
//     both the secrets vault (under `vault_key`) and the install
//     job's credentials list (so the credentials.json export
//     includes it).
//   - The function MUST be idempotent: calling it twice against the
//     same state must succeed without producing duplicate
//     artifacts. Failures are reported as `BootstrapError::*` for
//     the orchestrator to surface inline on the install-result page.

/// One artifact produced by a component's post-install bootstrap
/// step. The orchestrator persists this into the secrets vault and
/// the install job's credentials list.
#[derive(Debug, Clone)]
pub struct BootstrapArtifact {
    /// Vault key, e.g. "garage/lakekeeper-key-id". Conventionally
    /// `<component>/<purpose>` to keep the vault hierarchy clean.
    pub vault_key: String,
    /// Secret or opaque value. Always carried as `SecretString` so
    /// it can't accidentally land in a Display impl / log line.
    pub value: secrecy::SecretString,
    /// Human label rendered in the install-result page's credential
    /// list (e.g. "Garage Lakekeeper-scoped Access Key ID").
    pub label: String,
    /// Whether to display the value inline on the install-result
    /// page. `false` for non-sensitive metadata (warehouse names,
    /// bucket names, etc.) -- they still land in the vault + the
    /// credentials.json download, just don't clutter the inline
    /// list.
    pub display_inline: bool,
}

/// Failure modes for a post-install bootstrap step. Each variant
/// carries enough detail for the orchestrator to render a clear
/// inline message on the install-result page.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// Shelling to the component's admin CLI failed (process didn't
    /// start, binary not found, etc.). String is the original
    /// `io::Error` description.
    #[error("invoking component CLI failed: {0}")]
    Io(String),
    /// CLI ran but returned a non-zero exit. Carries the exit code
    /// (when available) and the stderr verbatim so the operator
    /// sees the actual error.
    #[error("component CLI exited {code:?}: {stderr}")]
    CliFailed { code: Option<i32>, stderr: String },
    /// CLI output didn't match the expected shape (regex didn't
    /// find the expected field). Surfaces with the raw output so
    /// upstream-CLI drift is debuggable.
    #[error("parsing component CLI output for {what}: expected pattern not found.\n--- output ---\n{output}")]
    ParseFailed { what: String, output: String },
    /// A bootstrap step detected pre-existing state it can't safely
    /// reconcile (e.g. Garage key already exists but vault has no
    /// secret for it -- we can't recover the secret because Garage
    /// redacts it on read). Operator must intervene.
    #[error("bootstrap state mismatch: {0}")]
    StateMismatch(String),
}

impl From<std::io::Error> for BootstrapError {
    fn from(e: std::io::Error) -> Self {
        BootstrapError::Io(e.to_string())
    }
}

// Component drivers (single-binary services built on top of
// `service::install_service`). Each module is a thin spec table.
pub mod databend;
pub mod garage;
pub mod grafana;
pub mod greptime;
pub mod kanidm;
pub mod lakekeeper;
pub mod openfga;
pub mod qdrant;
pub mod restate;
pub mod xtable;
