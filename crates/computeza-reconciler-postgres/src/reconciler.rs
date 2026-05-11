//! `PostgresReconciler` -- implements [`computeza_core::Reconciler`] against
//! a running PostgreSQL server.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use computeza_audit::{Action, AuditLog};
use computeza_core::{
    reconciler::{Context, Outcome},
    Driver, Error as CoreError, Health, NoOpDriver, Reconciler,
};
use computeza_state::{ResourceKey, SqliteStore, Store};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, info, warn};

use crate::{
    plan::{compute_plan, DatabaseChange, PostgresPlan, SYSTEM_DATABASES},
    resource::{PostgresInstance, PostgresSpec, PostgresStatus, ServerEndpoint},
};

/// Errors specific to the PostgreSQL reconciler.
///
/// Internal -- converted to [`computeza_core::Error`] before crossing the
/// trait boundary so the workspace's error API stays uniform.
#[derive(Debug, Error)]
pub enum PostgresError {
    /// SQLx-level failure (connection, query, decode).
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// The configured `sslmode` is not one of the libpq-recognised values.
    #[error("invalid sslmode: {0} (expected disable / allow / prefer / require / verify-ca / verify-full)")]
    InvalidSslMode(String),

    /// A database name in the spec contains characters the reconciler refuses
    /// to quote into SQL. Names must match `[A-Za-z0-9_-]+`.
    #[error("database name {0:?} contains characters outside [A-Za-z0-9_-]")]
    InvalidDatabaseName(String),
}

impl From<PostgresError> for CoreError {
    fn from(e: PostgresError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Reconciler for one PostgreSQL server instance.
///
/// Generic over the driver type (default [`NoOpDriver`]) because v0.0.2's
/// `apply` is purely SQL-driven and never invokes OS-level operations.
/// When the native driver lands and we need to (e.g.) restart Postgres
/// after a config-file change, callers will swap in the real driver.
pub struct PostgresReconciler<D: Driver = NoOpDriver> {
    endpoint: ServerEndpoint,
    superuser_password: SecretString,
    /// Lazy connection pool to the *administrative* database (always
    /// `postgres`). CREATE/DROP DATABASE statements run from here because
    /// they cannot run inside the database being dropped.
    pool: OnceCell<PgPool>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    /// Optional audit log; every successful DB change is appended here
    /// as a signed event. None means no audit (typical in tests).
    audit: Option<Arc<AuditLog>>,
    /// Optional state-store handle. When attached, every `observe()`
    /// success calls `Store::put_status` so the snapshot is durable.
    state: Option<StateBinding>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

/// Pair of (store, instance name) used to persist status. The instance
/// name is the `Resource::name` half of the key under which this
/// reconciler's `PostgresStatus` lands in the state store.
struct StateBinding {
    store: Arc<SqliteStore>,
    instance_name: String,
}

impl<D: Driver> PostgresReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and superuser
    /// password. The connection pool is opened lazily on first use.
    /// Builder methods [`with_audit`](Self::with_audit) and
    /// [`with_state`](Self::with_state) attach optional integrations.
    pub fn new(endpoint: ServerEndpoint, superuser_password: SecretString) -> Self {
        Self {
            endpoint,
            superuser_password,
            pool: OnceCell::new(),
            last_observed: Mutex::new(None),
            audit: None,
            state: None,
            _driver: std::marker::PhantomData,
        }
    }

    /// Attach an audit log. Every successful DB change in `apply` becomes
    /// a signed event in `audit`. Idempotent: re-calling replaces any
    /// previously-attached log.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Attach a state store + the instance name under which to persist
    /// the snapshot. Every successful `observe()` then calls
    /// `Store::put_status` so the snapshot survives a process restart
    /// and shows up in `GET /api/state/info`.
    #[must_use]
    pub fn with_state(mut self, store: Arc<SqliteStore>, instance_name: impl Into<String>) -> Self {
        self.state = Some(StateBinding {
            store,
            instance_name: instance_name.into(),
        });
        self
    }

    /// Back-compat shim. Prefer `new(..).with_audit(audit)` going forward.
    #[deprecated(note = "use PostgresReconciler::new(..).with_audit(..) instead")]
    pub fn new_with_audit(
        endpoint: ServerEndpoint,
        superuser_password: SecretString,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self::new(endpoint, superuser_password).with_audit(audit)
    }

    /// Get (or open) the lazy admin-database connection pool.
    async fn admin_pool(&self) -> Result<&PgPool, PostgresError> {
        self.pool
            .get_or_try_init(|| async {
                let opts = self
                    .endpoint
                    .to_connect_options(self.superuser_password.expose_secret(), "postgres")?;
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect_with(opts)
                    .await?;
                Ok::<_, PostgresError>(pool)
            })
            .await
    }

    /// Produce a status snapshot. Wraps the SQL queries that observe the
    /// running server.
    async fn snapshot(&self) -> Result<PostgresStatus, PostgresError> {
        let pool = self.admin_pool().await?;

        let version: String = sqlx::query("SELECT version()")
            .fetch_one(pool)
            .await?
            .try_get(0)?;

        // Filter out template / system databases so the status reflects what
        // the operator actually manages.
        let rows = sqlx::query(
            "SELECT datname FROM pg_database \
             WHERE datname NOT IN ('template0','template1','postgres') \
             ORDER BY datname",
        )
        .fetch_all(pool)
        .await?;

        let databases: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, _>(0))
            .collect::<Result<_, _>>()?;

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(PostgresStatus {
            server_version: Some(version),
            databases,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }

    /// Emit a signed audit event if an audit log is attached. Best-effort:
    /// failure to append is logged but does not fail the reconcile (the
    /// reconcile already committed to the database; rolling it back over
    /// an audit error would be more dangerous than carrying on).
    async fn audit_event(
        &self,
        action: Action,
        resource: Option<String>,
        detail: serde_json::Value,
    ) {
        let Some(log) = &self.audit else { return };
        if let Err(e) = log.append("system", action, resource, detail).await {
            warn!(error = %e, "failed to append audit event");
        }
    }

    /// If a state store is attached, persist the latest status snapshot
    /// under `postgres-instance/<instance_name>`. Best-effort: failure
    /// is logged but does not fail observe() (we already have the
    /// snapshot in memory; the persistence layer being temporarily
    /// unreachable is a degraded mode, not a fatal one).
    async fn persist_status(&self, status: &PostgresStatus) {
        let Some(binding) = &self.state else { return };
        let key = ResourceKey::cluster_scoped("postgres-instance", &binding.instance_name);
        let status_json = match serde_json::to_value(status) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to serialize PostgresStatus for state persistence");
                return;
            }
        };
        if let Err(e) = binding.store.put_status(&key, &status_json).await {
            warn!(
                error = %e,
                instance = %binding.instance_name,
                "failed to put_status; status survives in-memory only this cycle \
                 (check disk + SQLite file permissions; reconciler will retry on next observe)"
            );
        }
    }

    /// Validate a database identifier before we quote it into SQL. Strict
    /// allowlist: letters, digits, underscores, hyphens. PostgreSQL allows
    /// more, but the reconciler doesn't -- anything stranger should be a
    /// deliberate decision documented in the spec for that database, not
    /// something a YAML edit can accidentally introduce.
    fn validate_identifier(name: &str) -> Result<(), PostgresError> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(PostgresError::InvalidDatabaseName(name.to_string()));
        }
        Ok(())
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for PostgresReconciler<D> {
    type Resource = PostgresInstance;
    type Driver = D;
    type Plan = PostgresPlan;

    async fn observe(&self, _ctx: &Context) -> Result<PostgresStatus, CoreError> {
        let status = match self.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    error = %e,
                    "postgres observe failed; returning sentinel status with last_observe_failed=true. \
                     Reconciler will retry on next tick. Check: (1) network reachability to the \
                     endpoint, (2) credentials in the spec, (3) pg_isready on the target host."
                );
                PostgresStatus {
                    last_observe_failed: true,
                    ..PostgresStatus::default()
                }
            }
        };
        // Persist regardless of success/failure -- the failed-observe
        // sentinel is useful state too (it tells the UI to surface a drift
        // indicator). `persist_status` is best-effort; see its docs.
        self.persist_status(&status).await;
        Ok(status)
    }

    async fn plan(
        &self,
        desired: &PostgresSpec,
        actual: &PostgresStatus,
    ) -> Result<PostgresPlan, CoreError> {
        // Reject malformed identifiers up front so apply() can quote freely.
        for db in &desired.databases {
            Self::validate_identifier(&db.name).map_err(CoreError::from)?;
            if let Some(owner) = &db.owner {
                Self::validate_identifier(owner).map_err(CoreError::from)?;
            }
        }
        Ok(compute_plan(
            &desired.databases,
            &actual.databases,
            desired.prune,
        ))
    }

    async fn apply(
        &self,
        _ctx: &Context,
        plan: PostgresPlan,
        _driver: &D,
    ) -> Result<Outcome, CoreError> {
        if plan.is_empty() {
            return Ok(Outcome {
                changed: false,
                summary: "no changes".into(),
            });
        }
        let pool = self.admin_pool().await.map_err(CoreError::from)?;

        let mut applied = 0usize;
        // SQL-injection note: PostgreSQL does not allow parameter binding
        // for DDL identifiers, so CREATE / DROP DATABASE must interpolate
        // the name as a string. The defense is the validate_identifier()
        // allowlist (`[A-Za-z0-9_-]+` only) called immediately before
        // every interpolation site. Any identifier reaching this loop
        // has already been checked twice: once in plan() and again here.
        for change in &plan.changes {
            match change {
                DatabaseChange::Create(db) => {
                    Self::validate_identifier(&db.name).map_err(CoreError::from)?;
                    let mut sql = format!("CREATE DATABASE \"{}\"", db.name);
                    if let Some(owner) = &db.owner {
                        Self::validate_identifier(owner).map_err(CoreError::from)?;
                        sql.push_str(&format!(" OWNER \"{owner}\""));
                    }
                    if let Some(enc) = &db.encoding {
                        // Encoding values are an enum-ish set in Postgres; use
                        // a parameterised quoted literal to keep apostrophes
                        // safe even though we trust the source.
                        sql.push_str(&format!(" ENCODING '{}'", enc.replace('\'', "''")));
                    }
                    debug!(database = %db.name, "creating database");
                    sqlx::query(&sql)
                        .execute(pool)
                        .await
                        .map_err(PostgresError::from)
                        .map_err(CoreError::from)?;
                    self.audit_event(
                        Action::ResourceCreated,
                        Some(format!("postgres-instance/{}", db.name)),
                        json!({"operation": "CREATE DATABASE", "owner": db.owner, "encoding": db.encoding}),
                    )
                    .await;
                    applied += 1;
                }
                DatabaseChange::Drop { name } => {
                    Self::validate_identifier(name).map_err(CoreError::from)?;
                    if SYSTEM_DATABASES.contains(&name.as_str()) {
                        // Defence in depth -- compute_plan already filters these.
                        continue;
                    }
                    let sql = format!("DROP DATABASE \"{name}\"");
                    debug!(database = %name, "dropping database");
                    sqlx::query(&sql)
                        .execute(pool)
                        .await
                        .map_err(PostgresError::from)
                        .map_err(CoreError::from)?;
                    self.audit_event(
                        Action::ResourceDeleted,
                        Some(format!("postgres-instance/{name}")),
                        json!({"operation": "DROP DATABASE"}),
                    )
                    .await;
                    applied += 1;
                }
            }
        }

        // One overall Reconciled event with the summary.
        self.audit_event(
            Action::Reconciled,
            None,
            json!({"applied": applied, "changed": applied > 0}),
        )
        .await;

        info!(applied, "postgres reconcile applied");
        Ok(Outcome {
            changed: applied > 0,
            summary: format!("{applied} change(s) applied"),
        })
    }

    async fn health(&self, _ctx: &Context) -> Result<Health, CoreError> {
        // Healthy when we have observed successfully within the last 90s.
        // Otherwise Unknown -- actual unhealthiness is reported via the
        // status's `last_observe_failed` flag, which the UI surfaces as
        // amber until a fresh observe lands.
        let last = *self.last_observed.lock().await;
        match last {
            Some(t) if (Utc::now() - t).num_seconds() < 90 => Ok(Health::Healthy),
            Some(_) => Ok(Health::Degraded {
                reason: "stale observation".into(),
            }),
            None => Ok(Health::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_sql_injection_in_database_name() {
        let bad = "ok\"; DROP TABLE users;--";
        let err = PostgresReconciler::<NoOpDriver>::validate_identifier(bad).unwrap_err();
        assert!(
            matches!(err, PostgresError::InvalidDatabaseName(_)),
            "expected InvalidDatabaseName, got {err:?}"
        );
    }

    #[test]
    fn accepts_normal_database_name() {
        for name in ["analytics", "my_db", "prod-data", "X1"] {
            PostgresReconciler::<NoOpDriver>::validate_identifier(name)
                .unwrap_or_else(|e| panic!("expected {name} to validate, got {e}"));
        }
    }

    #[test]
    fn rejects_empty_database_name() {
        assert!(matches!(
            PostgresReconciler::<NoOpDriver>::validate_identifier(""),
            Err(PostgresError::InvalidDatabaseName(_))
        ));
    }

    #[test]
    fn rejects_unicode_database_name() {
        assert!(matches!(
            PostgresReconciler::<NoOpDriver>::validate_identifier("na\u{00ef}ve"),
            Err(PostgresError::InvalidDatabaseName(_))
        ));
    }
}
