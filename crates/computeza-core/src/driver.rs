//! Driver trait — abstracts the deployment target.
//!
//! Per spec §3.4, the same reconciler logic produces a Garage cluster on
//! native Linux, native macOS, native Windows, on Kubernetes, or on AWS EC2;
//! only the driver differs. v1.0 ships a single driver — `driver-native` —
//! covering all three OS families. Kubernetes and cloud drivers are
//! deferred to v1.2 (spec §3.2 Tier 2 table).
//!
//! New drivers can be added by partners without modifying core by
//! implementing this trait in a dynamically-loaded library.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Static description of a component the driver needs to deploy or update.
///
/// `kind` discriminates which managed component (e.g. "kanidm", "garage",
/// "lakekeeper"); `version` is the upstream version pin; `config` is the
/// component-specific configuration the reconciler computed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentSpec {
    /// Component kind (kebab-case, matches reconciler crate suffix).
    pub kind: String,
    /// Upstream version this deployment should run.
    pub version: String,
    /// Component-specific configuration as JSON. Each reconciler defines
    /// the schema; the driver passes it through opaquely.
    pub config: serde_json::Value,
}

/// Opaque handle to a deployed component instance. Returned by `deploy`,
/// passed back to `update` / `destroy` / `exec` / `logs` / `metrics`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    /// Stable identifier for this deployment within the driver.
    pub id: String,
    /// Component kind (mirrors `ComponentSpec.kind`).
    pub kind: String,
}

/// Request to execute a one-off command inside a deployment (used for
/// migrations, ad-hoc admin operations, debugging).
#[derive(Clone, Debug)]
pub struct ExecRequest {
    /// Command to run.
    pub command: Vec<String>,
    /// Optional environment overrides.
    pub env: Vec<(String, String)>,
}

/// Result of an exec call.
#[derive(Clone, Debug)]
pub struct ExecResponse {
    /// Process exit code.
    pub exit_code: i32,
    /// Combined stdout/stderr capture.
    pub output: String,
}

/// Options controlling a `logs` request.
#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    /// Number of trailing lines to return; `None` means stream from start.
    pub tail: Option<usize>,
    /// Whether to follow.
    pub follow: bool,
}

/// Streaming log handle. The exact stream type is driver-specific; this
/// alias is a placeholder until the streaming abstraction is locked.
pub type LogStream = Box<dyn std::io::Read + Send>;

/// Snapshot of metrics for a deployment.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Driver-specific metric blob.
    pub data: serde_json::Value,
}

/// The driver contract from spec §3.4. Native, K8s, and cloud drivers all
/// implement this against their respective runtimes.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Deploy a new component instance.
    async fn deploy(&self, spec: ComponentSpec) -> Result<Deployment>;

    /// Update an existing deployment's configuration.
    async fn update(&self, dep: &Deployment, spec: ComponentSpec) -> Result<()>;

    /// Destroy a deployment.
    async fn destroy(&self, dep: &Deployment) -> Result<()>;

    /// Execute a one-off command inside the deployment.
    async fn exec(&self, dep: &Deployment, cmd: ExecRequest) -> Result<ExecResponse>;

    /// Stream logs.
    async fn logs(&self, dep: &Deployment, opts: LogOptions) -> Result<LogStream>;

    /// Snapshot metrics.
    async fn metrics(&self, dep: &Deployment) -> Result<MetricsSnapshot>;
}

/// A driver that refuses every operation. Useful for reconcilers whose
/// `apply` step works purely against a managed component's API (typically
/// SQL or REST) and never needs OS-level deployment operations. Pairing
/// such a reconciler with `NoOpDriver` satisfies the [`crate::Reconciler`]
/// trait's `Driver` bound without conjuring a real driver.
///
/// Calling any method panics in debug builds and returns
/// [`crate::Error::Driver`] in release builds — the bug is "this reconciler
/// expected to never call the driver but did", and we want it loud.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOpDriver;

#[async_trait]
impl Driver for NoOpDriver {
    async fn deploy(&self, _spec: ComponentSpec) -> Result<Deployment> {
        Self::refuse("deploy")
    }
    async fn update(&self, _dep: &Deployment, _spec: ComponentSpec) -> Result<()> {
        Self::refuse("update")
    }
    async fn destroy(&self, _dep: &Deployment) -> Result<()> {
        Self::refuse("destroy")
    }
    async fn exec(&self, _dep: &Deployment, _cmd: ExecRequest) -> Result<ExecResponse> {
        Self::refuse("exec")
    }
    async fn logs(&self, _dep: &Deployment, _opts: LogOptions) -> Result<LogStream> {
        Self::refuse("logs")
    }
    async fn metrics(&self, _dep: &Deployment) -> Result<MetricsSnapshot> {
        Self::refuse("metrics")
    }
}

impl NoOpDriver {
    fn refuse<T>(op: &str) -> Result<T> {
        debug_assert!(false, "NoOpDriver::{op} called — reconciler should not invoke driver");
        Err(crate::Error::Driver(format!(
            "NoOpDriver does not support {op}"
        )))
    }
}
