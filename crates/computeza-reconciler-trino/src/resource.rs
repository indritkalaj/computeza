//! `TrinoInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use serde::{Deserialize, Serialize};

/// A running Trino coordinator managed by Computeza.
pub struct TrinoInstance;

impl Resource for TrinoInstance {
    type Spec = TrinoSpec;
    type Status = TrinoStatus;

    fn kind() -> &'static str {
        "trino-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrinoSpec {
    /// Trino HTTP endpoint.
    pub endpoint: TrinoEndpoint,
}

/// How to reach a Trino coordinator. Same shape as every other
/// reconciler's Endpoint type so the dispatch_install spec writer
/// in ui-server doesn't need a per-component special case.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrinoEndpoint {
    /// HTTP base URL, e.g. `http://127.0.0.1:8088`.
    pub base_url: String,
    /// Skip TLS verification (development only). Kept for spec
    /// uniformity with the other Endpoint types; unused for plain
    /// HTTP installs.
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrinoStatus {
    /// True if the Trino HTTP coordinator port accepted a TCP
    /// connection on the most recent observation.
    pub reachable: bool,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
