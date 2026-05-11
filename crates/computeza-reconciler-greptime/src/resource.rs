//! `GreptimeInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running GreptimeDB cluster managed by Computeza.
pub struct GreptimeInstance;

impl Resource for GreptimeInstance {
    type Spec = GreptimeSpec;
    type Status = GreptimeStatus;

    fn kind() -> &'static str {
        "greptime-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GreptimeSpec {
    /// GreptimeDB HTTP API endpoint.
    pub endpoint: GreptimeEndpoint,
    /// Admin bearer token. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the GreptimeDB HTTP API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GreptimeEndpoint {
    /// Base URL -- typically `http://greptime:4000`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GreptimeStatus {
    /// Server version reported by `/health`.
    pub server_version: Option<String>,
    /// Whether the cluster reports itself healthy via /health.
    pub ready: Option<bool>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
