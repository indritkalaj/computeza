//! `SailInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use serde::{Deserialize, Serialize};

/// A running Sail Spark-Connect server managed by Computeza.
pub struct SailInstance;

impl Resource for SailInstance {
    type Spec = SailSpec;
    type Status = SailStatus;

    fn kind() -> &'static str {
        "sail-instance"
    }
}

/// User-declared desired state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SailSpec {
    /// Sail Spark Connect endpoint.
    pub endpoint: SailEndpoint,
}

/// How to reach a Sail Spark Connect server. Stored as `base_url`
/// (e.g. `http://127.0.0.1:50051`) to match the convention every
/// other reconciler (databend, lakekeeper, qdrant, ...) uses, so
/// the dispatch_install spec writer in ui-server doesn't need a
/// per-component special case. Spark clients consume an `sc://`
/// URI, which we synthesise from the parsed host + port; the
/// liveness probe uses the raw host:port. Both helpers tolerate a
/// missing scheme or port and fall back to sensible defaults.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SailEndpoint {
    /// HTTP-style base URL, e.g. `http://127.0.0.1:50051`. Spark
    /// Connect itself speaks gRPC over the same host:port; storing
    /// the URL keeps the on-disk shape uniform with the other
    /// reconcilers' specs.
    pub base_url: String,
    /// Skip TLS verification (development only). Matches the same
    /// field on every other Endpoint type; unused for plain HTTP
    /// but threaded through so the spec deserialises uniformly.
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

impl SailEndpoint {
    /// Build the `sc://<host>:<port>` URI expected by
    /// `SparkSession.builder.remote()`. Falls back to localhost:50051
    /// if base_url isn't a parseable `http(s)://host:port[/path]`.
    pub fn spark_connect_uri(&self) -> String {
        let (host, port) = self.parse_host_port();
        format!("sc://{host}:{port}")
    }

    /// Build the bare TCP target used by the liveness probe.
    pub fn tcp_target(&self) -> String {
        let (host, port) = self.parse_host_port();
        format!("{host}:{port}")
    }

    fn parse_host_port(&self) -> (String, u16) {
        let s = self
            .base_url
            .strip_prefix("http://")
            .or_else(|| self.base_url.strip_prefix("https://"))
            .unwrap_or(&self.base_url);
        let s = s.split('/').next().unwrap_or("");
        match s.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() => (
                h.to_string(),
                p.parse::<u16>().unwrap_or(50051),
            ),
            _ => ("127.0.0.1".to_string(), 50051),
        }
    }
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SailStatus {
    /// True if the Spark Connect gRPC port accepted a TCP connection
    /// on the most recent observation.
    pub reachable: bool,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
