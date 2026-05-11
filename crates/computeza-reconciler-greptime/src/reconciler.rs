//! `GreptimeReconciler` -- read-only HTTP reconciler.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use computeza_core::{
    reconciler::{Context, Outcome},
    Driver, Error as CoreError, Health, NoOpDriver, Reconciler,
};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, warn};

use crate::resource::{GreptimeEndpoint, GreptimeInstance, GreptimeSpec, GreptimeStatus};

/// GreptimeDB-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum GreptimeError {
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
    #[error("greptime responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<GreptimeError> for CoreError {
    fn from(e: GreptimeError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one GreptimeDB cluster.
pub struct GreptimeReconciler<D: Driver = NoOpDriver> {
    endpoint: GreptimeEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

impl<D: Driver> GreptimeReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and admin token.
    pub fn new(endpoint: GreptimeEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            _driver: std::marker::PhantomData,
        }
    }

    fn build_client(insecure: bool) -> Result<reqwest::Client, GreptimeError> {
        if insecure {
            warn!("greptime: insecure_skip_tls_verify=true; TLS validation disabled");
        }
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-greptime/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, GreptimeError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, GreptimeError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| GreptimeError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| GreptimeError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, GreptimeError> {
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
            return Err(GreptimeError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<GreptimeStatus, GreptimeError> {
        // GreptimeDB exposes /health for liveness and /status (or /version
        // depending on the build) for version metadata.
        let health = self.get_json("/health").await.ok();
        let ready = health.as_ref().map(|_| true);

        let status_obj = self.get_json("/status").await.ok();
        let server_version = status_obj
            .as_ref()
            .and_then(|v| v.get("version"))
            .and_then(|x| x.as_str())
            .map(str::to_string);

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(GreptimeStatus {
            server_version,
            ready,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for GreptimeReconciler<D> {
    type Resource = GreptimeInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<GreptimeStatus, CoreError> {
        match self.snapshot().await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(error = %e, "greptime observe failed");
                Ok(GreptimeStatus {
                    last_observe_failed: true,
                    ..GreptimeStatus::default()
                })
            }
        }
    }

    async fn plan(
        &self,
        _desired: &GreptimeSpec,
        _actual: &GreptimeStatus,
    ) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("greptime apply: no-op (read-only at v0.0.x)");
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

    fn r(base: &str) -> GreptimeReconciler<NoOpDriver> {
        GreptimeReconciler::new(
            GreptimeEndpoint {
                base_url: base.into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        )
    }

    #[test]
    fn url_join_works() {
        assert_eq!(
            r("http://greptime:4000").url("/health").unwrap(),
            "http://greptime:4000/health"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        assert!(matches!(
            r("not a url").url("/x"),
            Err(GreptimeError::InvalidUrl(_))
        ));
    }
}
