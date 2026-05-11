//! `OpenFgaInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running OpenFGA instance managed by Computeza.
pub struct OpenFgaInstance;

impl Resource for OpenFgaInstance {
    type Spec = OpenFgaSpec;
    type Status = OpenFgaStatus;

    fn kind() -> &'static str {
        "openfga-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenFgaSpec {
    /// OpenFGA HTTP playground API endpoint.
    pub endpoint: OpenFgaEndpoint,
    /// Pre-shared bearer token. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the OpenFGA HTTP API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenFgaEndpoint {
    /// Base URL -- typically `http://openfga:8080`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenFgaStatus {
    /// Server version reported by `/healthz` (when shipped by the build).
    pub server_version: Option<String>,
    /// Number of configured stores.
    pub store_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
