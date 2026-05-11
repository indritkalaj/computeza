//! `QdrantInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Qdrant cluster managed by Computeza.
pub struct QdrantInstance;

impl Resource for QdrantInstance {
    type Spec = QdrantSpec;
    type Status = QdrantStatus;

    fn kind() -> &'static str {
        "qdrant-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QdrantSpec {
    /// Qdrant admin HTTP endpoint.
    pub endpoint: QdrantEndpoint,
    /// Admin API key. Skipped from (de)serialization.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach the Qdrant admin API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QdrantEndpoint {
    /// Base URL -- typically `http://qdrant:6333`.
    pub base_url: String,
    /// Skip TLS verification (development only).
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QdrantStatus {
    /// Server version reported by `/`.
    pub server_version: Option<String>,
    /// Number of collections in the cluster.
    pub collection_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
