//! PostgresInstance resource type.

use chrono::{DateTime, Utc};
use computeza_core::Resource;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// A PostgreSQL server instance managed by Computeza.
///
/// Per the boundary in this crate's module docs, "managed" here means
/// configured at the SQL layer: databases, users, migrations. The OS-level
/// installation of the server is the driver's responsibility.
pub struct PostgresInstance;

impl Resource for PostgresInstance {
    type Spec = PostgresSpec;
    type Status = PostgresStatus;

    fn kind() -> &'static str {
        "postgres-instance"
    }
}

/// User-declared desired state for a PostgreSQL instance.
///
/// `endpoint` and `superuser_password` together identify how to reach the
/// running server; `databases` is the desired set of databases on it.
/// Anything else (users, schemas, migrations) is out of scope for v0.0.2.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PostgresSpec {
    /// How to reach the running server.
    pub endpoint: ServerEndpoint,
    /// Superuser credentials. Wrapped in `SecretString` so it doesn't leak
    /// via `Debug` / `Display` / serde JSON output.
    pub superuser_password: SecretString,
    /// Databases that should exist on the server. Reconciler creates any
    /// missing entries and (when `prune` is true) drops any extras.
    pub databases: Vec<DatabaseSpec>,
    /// When true, drop databases that exist on the server but are not in
    /// `databases`. Defaults to false — destructive operations should be
    /// opt-in.
    #[serde(default)]
    pub prune: bool,
}

/// Where to reach a running PostgreSQL server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerEndpoint {
    /// Hostname or IP.
    pub host: String,
    /// TCP port (default 5432).
    pub port: u16,
    /// Superuser to connect as (typically `postgres`).
    pub superuser: String,
    /// Optional sslmode to apply to the connection string. Accepts the
    /// standard libpq values ("disable", "require", "verify-ca",
    /// "verify-full").
    #[serde(default)]
    pub sslmode: Option<String>,
}

impl ServerEndpoint {
    /// Build sqlx `PgConnectOptions` for this endpoint with the given
    /// password and target database. Kept crate-private so the unwrapped
    /// password never escapes the reconciler.
    pub(crate) fn to_connect_options(
        &self,
        password: &str,
        database: &str,
    ) -> Result<sqlx::postgres::PgConnectOptions, crate::reconciler::PostgresError> {
        use sqlx::postgres::{PgConnectOptions, PgSslMode};

        let mut opts = PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .username(&self.superuser)
            .password(password)
            .database(database);

        if let Some(mode) = &self.sslmode {
            let parsed = match mode.as_str() {
                "disable" => PgSslMode::Disable,
                "allow" => PgSslMode::Allow,
                "prefer" => PgSslMode::Prefer,
                "require" => PgSslMode::Require,
                "verify-ca" => PgSslMode::VerifyCa,
                "verify-full" => PgSslMode::VerifyFull,
                other => {
                    return Err(crate::reconciler::PostgresError::InvalidSslMode(
                        other.to_string(),
                    ))
                }
            };
            opts = opts.ssl_mode(parsed);
        }
        Ok(opts)
    }
}

/// One desired database on the server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DatabaseSpec {
    /// Database name. Must be a valid PostgreSQL identifier; the reconciler
    /// quotes it but rejects anything outside `[A-Za-z0-9_-]+` to avoid
    /// surprises.
    pub name: String,
    /// Optional owner role (must already exist on the server). When `None`,
    /// Postgres assigns the connecting superuser as owner.
    #[serde(default)]
    pub owner: Option<String>,
    /// Optional encoding (default Postgres template encoding).
    #[serde(default)]
    pub encoding: Option<String>,
}

/// System-observed actual state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PostgresStatus {
    /// Server version as reported by `version()`. None if observe has
    /// never succeeded.
    pub server_version: Option<String>,
    /// Names of databases present on the server (excluding the always-present
    /// `template0`, `template1`, `postgres` system DBs).
    pub databases: Vec<String>,
    /// When the last successful observation completed.
    pub last_observed_at: Option<DateTime<Utc>>,
    /// Whether the most recent observe attempt failed.
    pub last_observe_failed: bool,
}

impl Default for PostgresStatus {
    fn default() -> Self {
        Self {
            server_version: None,
            databases: Vec::new(),
            last_observed_at: None,
            last_observe_failed: false,
        }
    }
}
