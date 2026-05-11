//! `GrafanaInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Grafana instance managed by Computeza.
pub struct GrafanaInstance;

impl Resource for GrafanaInstance {
    type Spec = GrafanaSpec;
    type Status = GrafanaStatus;

    fn kind() -> &'static str {
        "grafana-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrafanaSpec {
    /// Grafana HTTP endpoint.
    pub endpoint: GrafanaEndpoint,
    /// Admin bearer token (a Grafana API key). Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Grafana HTTP API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrafanaEndpoint {
    /// Base URL -- typically `http://grafana:3000`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GrafanaStatus {
    /// Server version reported by `/api/health`.
    pub server_version: Option<String>,
    /// Number of registered datasources.
    pub datasource_count: Option<u64>,
    /// Whether `/api/health` reports OK.
    pub ready: Option<bool>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
