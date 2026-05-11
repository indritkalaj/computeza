//! `DatabendInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Databend cluster managed by Computeza.
pub struct DatabendInstance;

impl Resource for DatabendInstance {
    type Spec = DatabendSpec;
    type Status = DatabendStatus;

    fn kind() -> &'static str {
        "databend-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabendSpec {
    /// Databend admin HTTP endpoint.
    pub endpoint: DatabendEndpoint,
    /// Admin bearer token. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Databend admin API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabendEndpoint {
    /// Base URL -- typically `http://databend-meta:28101`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DatabendStatus {
    /// Server version reported by the admin API.
    pub server_version: Option<String>,
    /// Number of query nodes registered with the meta service.
    pub query_node_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
