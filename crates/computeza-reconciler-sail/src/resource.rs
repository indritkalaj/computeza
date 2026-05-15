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

/// How to reach a Sail Spark Connect server. Sail listens via gRPC;
/// the URL convention used by PySpark clients is `sc://<host>:<port>`.
/// Computeza stores the bare host + port and reconstructs both the
/// `sc://` URI (for clients) and the TCP target (for liveness probes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SailEndpoint {
    /// Host the Sail server binds. Typically 127.0.0.1 for the
    /// single-node local install path.
    pub host: String,
    /// Spark Connect gRPC port. Default 50051.
    pub port: u16,
}

impl SailEndpoint {
    /// Build the `sc://<host>:<port>` URI expected by `SparkSession.builder.remote()`.
    pub fn spark_connect_uri(&self) -> String {
        format!("sc://{}:{}", self.host, self.port)
    }

    /// Build the bare TCP target used by the liveness probe.
    pub fn tcp_target(&self) -> String {
        format!("{}:{}", self.host, self.port)
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
