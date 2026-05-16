//! `TrinoReconciler` -- TCP-liveness reconciler for the Trino coordinator.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use computeza_core::{
    reconciler::{Context, Outcome},
    Driver, Error as CoreError, Health, NoOpDriver, Reconciler,
};
use computeza_state::{ResourceKey, SqliteStore, Store};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::resource::{TrinoEndpoint, TrinoInstance, TrinoSpec, TrinoStatus};

/// Trino-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum TrinoError {
    /// TCP probe failed (timeout or refused).
    #[error("could not reach Trino at {target}: {source}")]
    Unreachable {
        /// host:port we tried to connect to
        target: String,
        /// underlying error
        source: std::io::Error,
    },
}

impl From<TrinoError> for CoreError {
    fn from(e: TrinoError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Trino coordinator. The Studio
/// editor's query path exercises Trino end-to-end on every
/// operator query, so the reconciler only needs to confirm the
/// coordinator is up at all (TCP liveness on the HTTP port).
pub struct TrinoReconciler<D: Driver = NoOpDriver> {
    endpoint: TrinoEndpoint,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    state: Option<StateBinding>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

struct StateBinding {
    store: Arc<SqliteStore>,
    instance_name: String,
}

impl<D: Driver> TrinoReconciler<D> {
    /// Construct a reconciler bound to the given Trino HTTP endpoint.
    pub fn new(endpoint: TrinoEndpoint) -> Self {
        Self {
            endpoint,
            last_observed: Mutex::new(None),
            state: None,
            _driver: std::marker::PhantomData,
        }
    }

    /// Attach a state store. Mirrors the DatabendReconciler convention.
    #[must_use]
    pub fn with_state(mut self, store: Arc<SqliteStore>, instance_name: impl Into<String>) -> Self {
        self.state = Some(StateBinding {
            store,
            instance_name: instance_name.into(),
        });
        self
    }

    async fn persist_status(&self, status: &TrinoStatus) {
        let Some(binding) = &self.state else { return };
        let key = ResourceKey::cluster_scoped("trino-instance", &binding.instance_name);
        let status_json = match serde_json::to_value(status) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to serialize TrinoStatus");
                return;
            }
        };
        if let Err(e) = binding.store.put_status(&key, &status_json).await {
            warn!(
                error = %e,
                instance = %binding.instance_name,
                "failed to put_status for trino; status survives in-memory only this cycle"
            );
        }
    }

    fn tcp_target(&self) -> String {
        // Parse host:port from base_url. Falls back to localhost:8088
        // on malformed input rather than crashing the reconciler tick.
        let s = self
            .endpoint
            .base_url
            .strip_prefix("http://")
            .or_else(|| self.endpoint.base_url.strip_prefix("https://"))
            .unwrap_or(&self.endpoint.base_url);
        let s = s.split('/').next().unwrap_or("");
        match s.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && !p.is_empty() => format!("{h}:{p}"),
            _ => "127.0.0.1:8088".to_string(),
        }
    }

    async fn snapshot(&self) -> TrinoStatus {
        let target = self.tcp_target();
        // 3-second TCP-connect ceiling: Trino accepts immediately
        // when healthy; a 3s budget fails fast on a hung interface
        // without making the reconciler tick visibly slow.
        let reachable = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&target),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);
        TrinoStatus {
            reachable,
            last_observed_at: Some(now),
            last_observe_failed: !reachable,
        }
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for TrinoReconciler<D> {
    type Resource = TrinoInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<TrinoStatus, CoreError> {
        let status = self.snapshot().await;
        self.persist_status(&status).await;
        Ok(status)
    }

    async fn plan(&self, _desired: &TrinoSpec, _actual: &TrinoStatus) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("trino apply: no-op (read-only at v0.0.x)");
        Ok(Outcome {
            changed: false,
            summary: "read-only".into(),
        })
    }

    async fn health(&self, _ctx: &Context) -> Result<Health, CoreError> {
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

    fn r(base_url: &str) -> TrinoReconciler<NoOpDriver> {
        TrinoReconciler::new(TrinoEndpoint {
            base_url: base_url.into(),
            insecure_skip_tls_verify: false,
        })
    }

    #[test]
    fn tcp_target_parses_base_url() {
        let rec = r("http://127.0.0.1:8088");
        assert_eq!(rec.tcp_target(), "127.0.0.1:8088");
    }

    #[test]
    fn tcp_target_falls_back_when_malformed() {
        let rec = r("");
        assert_eq!(rec.tcp_target(), "127.0.0.1:8088");
    }
}
