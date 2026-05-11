//! `LakekeeperInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Lakekeeper instance managed by Computeza.
pub struct LakekeeperInstance;

impl Resource for LakekeeperInstance {
    type Spec = LakekeeperSpec;
    type Status = LakekeeperStatus;

    fn kind() -> &'static str {
        "lakekeeper-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LakekeeperSpec {
    /// Lakekeeper management API endpoint.
    pub endpoint: LakekeeperEndpoint,
    /// Admin bearer token. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Lakekeeper management API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LakekeeperEndpoint {
    /// Base URL — typically `https://catalog.example.com`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LakekeeperStatus {
    /// Server version reported by the management API.
    pub server_version: Option<String>,
    /// Number of projects visible to the admin token.
    pub project_count: Option<u64>,
    /// Number of warehouses visible to the admin token.
    pub warehouse_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
