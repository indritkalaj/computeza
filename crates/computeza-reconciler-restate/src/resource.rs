//! `RestateInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Restate cluster managed by Computeza.
pub struct RestateInstance;

impl Resource for RestateInstance {
    type Spec = RestateSpec;
    type Status = RestateStatus;

    fn kind() -> &'static str {
        "restate-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestateSpec {
    /// Restate admin HTTP endpoint.
    pub endpoint: RestateEndpoint,
    /// Admin bearer token. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Restate admin API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestateEndpoint {
    /// Base URL -- typically `http://restate:9070`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestateStatus {
    /// Server version reported by `/version`.
    pub server_version: Option<String>,
    /// Number of registered services.
    pub service_count: Option<u64>,
    /// Number of registered deployments.
    pub deployment_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
