//! `GrafanaReconciler` -- read-only HTTP reconciler.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use computeza_core::{
    reconciler::{Context, Outcome},
    Driver, Error as CoreError, Health, NoOpDriver, Reconciler,
};
use computeza_state::{ResourceKey, SqliteStore, Store};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, warn};

use crate::resource::{GrafanaEndpoint, GrafanaInstance, GrafanaSpec, GrafanaStatus};

/// Grafana-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum GrafanaError {
    /// HTTP transport failure.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// JSON decode failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Endpoint URL is malformed.
    #[error("invalid base_url: {0}")]
    InvalidUrl(String),
    /// Server returned a non-2xx status.
    #[error("grafana responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<GrafanaError> for CoreError {
    fn from(e: GrafanaError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Grafana instance.
pub struct GrafanaReconciler<D: Driver = NoOpDriver> {
    endpoint: GrafanaEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    /// Optional state-store handle. See `KanidmReconciler`.
    state: Option<StateBinding>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

/// Pair of (store, instance name) used to persist status.
struct StateBinding {
    store: Arc<SqliteStore>,
    instance_name: String,
}

impl<D: Driver> GrafanaReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and API key.
    pub fn new(endpoint: GrafanaEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            state: None,
            _driver: std::marker::PhantomData,
        }
    }

    /// Attach a state store. See `KanidmReconciler::with_state`.
    #[must_use]
    pub fn with_state(mut self, store: Arc<SqliteStore>, instance_name: impl Into<String>) -> Self {
        self.state = Some(StateBinding {
            store,
            instance_name: instance_name.into(),
        });
        self
    }

    /// Persist the latest status under `grafana-instance/<instance_name>`
    /// if a state store is attached. Best-effort.
    async fn persist_status(&self, status: &GrafanaStatus) {
        let Some(binding) = &self.state else { return };
        let key = ResourceKey::cluster_scoped("grafana-instance", &binding.instance_name);
        let status_json = match serde_json::to_value(status) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to serialize GrafanaStatus for state persistence");
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

    fn build_client(insecure: bool) -> Result<reqwest::Client, GrafanaError> {
        if insecure {
            warn!("grafana: insecure_skip_tls_verify=true; TLS validation disabled");
        }
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-grafana/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, GrafanaError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, GrafanaError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| GrafanaError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| GrafanaError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, GrafanaError> {
        let url = self.url(path)?;
        let token = self.admin_token.expose_secret();
        let resp = self
            .http()
            .await?
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(GrafanaError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<GrafanaStatus, GrafanaError> {
        let health = self.get_json("/api/health").await.ok();
        let server_version = health
            .as_ref()
            .and_then(|v| v.get("version"))
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let ready = health
            .as_ref()
            .and_then(|v| v.get("database"))
            .and_then(|x| x.as_str())
            .map(|s| s == "ok");

        let datasource_count = match self.get_json("/api/datasources").await {
            Ok(v) => v.as_array().map(|a| a.len() as u64),
            Err(_) => None,
        };

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(GrafanaStatus {
            server_version,
            datasource_count,
            ready,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for GrafanaReconciler<D> {
    type Resource = GrafanaInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<GrafanaStatus, CoreError> {
        let status = match self.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    error = %e,
                    "grafana observe failed; returning sentinel status with last_observe_failed=true. \
                     Reconciler will retry on next tick. Check: (1) /api/health reachable, \
                     (2) admin token / service-account key valid, (3) Grafana DB healthy."
                );
                GrafanaStatus {
                    last_observe_failed: true,
                    ..GrafanaStatus::default()
                }
            }
        };
        self.persist_status(&status).await;
        Ok(status)
    }

    async fn plan(&self, _desired: &GrafanaSpec, _actual: &GrafanaStatus) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("grafana apply: no-op (read-only at v0.0.x)");
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

    fn r(base: &str) -> GrafanaReconciler<NoOpDriver> {
        GrafanaReconciler::new(
            GrafanaEndpoint {
                base_url: base.into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        )
    }

    #[test]
    fn url_join_works() {
        assert_eq!(
            r("http://grafana:3000").url("/api/health").unwrap(),
            "http://grafana:3000/api/health"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        assert!(matches!(
            r("not a url").url("/x"),
            Err(GrafanaError::InvalidUrl(_))
        ));
    }
}
