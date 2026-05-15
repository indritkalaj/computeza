//! `SailReconciler` -- TCP-liveness reconciler for Spark Connect.

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

use crate::resource::{SailEndpoint, SailInstance, SailSpec, SailStatus};

/// Sail-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum SailError {
    /// TCP probe failed (timeout or refused).
    #[error("could not reach Sail Spark Connect at {target}: {source}")]
    Unreachable {
        /// host:port we tried to connect to
        target: String,
        /// underlying error
        source: std::io::Error,
    },
}

impl From<SailError> for CoreError {
    fn from(e: SailError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Sail Spark Connect server. Confirms
/// the gRPC port is accepting TCP connections; protocol-level
/// validation is deferred until Computeza has a use case that needs
/// it (the Studio Python execution path validates end-to-end via
/// actual SparkSession queries, which is more authoritative than any
/// reachability ping).
pub struct SailReconciler<D: Driver = NoOpDriver> {
    endpoint: SailEndpoint,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    state: Option<StateBinding>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

struct StateBinding {
    store: Arc<SqliteStore>,
    instance_name: String,
}

impl<D: Driver> SailReconciler<D> {
    /// Construct a reconciler bound to the given Spark Connect endpoint.
    pub fn new(endpoint: SailEndpoint) -> Self {
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

    async fn persist_status(&self, status: &SailStatus) {
        let Some(binding) = &self.state else { return };
        let key = ResourceKey::cluster_scoped("sail-instance", &binding.instance_name);
        let status_json = match serde_json::to_value(status) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to serialize SailStatus");
                return;
            }
        };
        if let Err(e) = binding.store.put_status(&key, &status_json).await {
            warn!(
                error = %e,
                instance = %binding.instance_name,
                "failed to put_status for sail; status survives in-memory only this cycle"
            );
        }
    }

    async fn snapshot(&self) -> SailStatus {
        let target = self.endpoint.tcp_target();
        // 3-second timeout: Spark Connect responds to TCP SYN
        // instantly on a healthy node; a 3s ceiling fails fast on
        // a hung interface without making the reconciler tick
        // visibly slow.
        let reachable = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(&target),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);
        SailStatus {
            reachable,
            last_observed_at: Some(now),
            last_observe_failed: !reachable,
        }
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for SailReconciler<D> {
    type Resource = SailInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<SailStatus, CoreError> {
        let status = self.snapshot().await;
        self.persist_status(&status).await;
        Ok(status)
    }

    async fn plan(
        &self,
        _desired: &SailSpec,
        _actual: &SailStatus,
    ) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("sail apply: no-op (read-only at v0.0.x)");
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

    fn r(base_url: &str) -> SailReconciler<NoOpDriver> {
        SailReconciler::new(SailEndpoint {
            base_url: base_url.into(),
            insecure_skip_tls_verify: false,
        })
    }

    #[test]
    fn endpoint_builds_spark_connect_uri() {
        let rec = r("http://127.0.0.1:50051");
        assert_eq!(rec.endpoint.spark_connect_uri(), "sc://127.0.0.1:50051");
        assert_eq!(rec.endpoint.tcp_target(), "127.0.0.1:50051");
    }

    #[test]
    fn endpoint_falls_back_when_base_url_malformed() {
        let rec = r("");
        assert_eq!(rec.endpoint.spark_connect_uri(), "sc://127.0.0.1:50051");
    }
}
