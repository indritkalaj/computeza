//! `GarageInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Garage cluster managed by Computeza.
pub struct GarageInstance;

impl Resource for GarageInstance {
    type Spec = GarageSpec;
    type Status = GarageStatus;

    fn kind() -> &'static str {
        "garage-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GarageSpec {
    /// Garage admin endpoint (typically `http://node:3903`).
    pub endpoint: GarageEndpoint,
    /// Admin bearer token. Skipped from (de)serialization — see the
    /// secrets-store pattern documented in `computeza-reconciler-postgres`.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Garage admin API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GarageEndpoint {
    /// Base URL — typically `http://garage-node:3903`.
    pub base_url: String,
    /// Whether to skip TLS verification. Default false.
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GarageStatus {
    /// Garage version reported by `/v1/status`.
    pub server_version: Option<String>,
    /// Number of nodes in the cluster layout, when discoverable.
    pub node_count: Option<u64>,
    /// Number of buckets visible to the admin token.
    pub bucket_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
