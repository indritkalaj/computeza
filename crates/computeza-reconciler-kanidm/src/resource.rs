//! `KanidmInstance` resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A running Kanidm server instance managed by Computeza.
pub struct KanidmInstance;

impl Resource for KanidmInstance {
    type Spec = KanidmSpec;
    type Status = KanidmStatus;

    fn kind() -> &'static str {
        "kanidm-instance"
    }
}

/// User-declared desired state.
///
/// v0.0.x carries only connection details and a managed-realm marker.
/// Future iterations add `users`, `groups`, `oauth2_clients`,
/// `passkey_policies`, `federation` collections.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KanidmSpec {
    /// How to reach the running Kanidm server.
    pub endpoint: KanidmEndpoint,
    /// Bearer token for the admin/idm_admin account. Skipped from
    /// (de)serialization — see the same secrets-store pattern documented
    /// in the Postgres reconciler.
    #[serde(skip, default = "default_secret")]
    pub admin_token: SecretString,
}

fn default_secret() -> SecretString {
    SecretString::from(String::new())
}

/// How to reach a Kanidm server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KanidmEndpoint {
    /// Base URL — typically `https://idm.example.com`.
    pub base_url: String,
    /// Whether to skip TLS verification. Default false. Set true only for
    /// development against a self-signed cert.
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

/// System-observed actual state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KanidmStatus {
    /// Server version reported by Kanidm. None if observe has never
    /// succeeded.
    pub server_version: Option<String>,
    /// Number of groups visible to the admin token.
    pub group_count: Option<u64>,
    /// Number of persons (human accounts) visible to the admin token.
    pub person_count: Option<u64>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}
