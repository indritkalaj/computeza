//! `GarageReconciler` -- implements [`computeza_core::Reconciler`] against
//! a running Garage cluster.
//!
//! v0.0.x is read-only and structurally mirrors `KanidmReconciler` in
//! `computeza-reconciler-kanidm`. The HTTP-reconciler pattern is enforced
//! by repetition -- every reconciler that talks to an HTTP admin API
//! follows the same shape so reading one is reading them all.

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

use crate::resource::{GarageEndpoint, GarageInstance, GarageSpec, GarageStatus};

/// Garage-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum GarageError {
    /// HTTP transport failure.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// JSON decode failure on a Garage response.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Endpoint URL is malformed.
    #[error("invalid base_url: {0}")]
    InvalidUrl(String),
    /// Server returned a non-2xx status.
    #[error("garage responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<GarageError> for CoreError {
    fn from(e: GarageError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Garage cluster.
pub struct GarageReconciler<D: Driver = NoOpDriver> {
    endpoint: GarageEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

impl<D: Driver> GarageReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and admin token.
    pub fn new(endpoint: GarageEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            _driver: std::marker::PhantomData,
        }
    }

    fn build_client(insecure: bool) -> Result<reqwest::Client, GarageError> {
        if insecure {
            warn!("garage: insecure_skip_tls_verify=true; TLS validation disabled");
        }
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-garage/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, GarageError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, GarageError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| GarageError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| GarageError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, GarageError> {
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
            return Err(GarageError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<GarageStatus, GarageError> {
        // Garage's admin API exposes `/v1/status` and `/v1/bucket`. The
        // exact response shape evolves across releases; we tolerate the
        // common variations and leave unmappable fields as None.
        let status_obj = self.get_json("/v1/status").await.ok();
        let server_version = status_obj
            .as_ref()
            .and_then(|v| v.get("garageVersion").or_else(|| v.get("version")))
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let node_count = status_obj
            .as_ref()
            .and_then(|v| v.get("knownNodes").or_else(|| v.get("nodes")))
            .and_then(|x| x.as_array())
            .map(|a| a.len() as u64);

        let bucket_count = match self.get_json("/v1/bucket").await {
            Ok(v) => v.as_array().map(|a| a.len() as u64),
            Err(_) => None,
        };

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(GarageStatus {
            server_version,
            node_count,
            bucket_count,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for GarageReconciler<D> {
    type Resource = GarageInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<GarageStatus, CoreError> {
        match self.snapshot().await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(error = %e, "garage observe failed");
                Ok(GarageStatus {
                    last_observe_failed: true,
                    ..GarageStatus::default()
                })
            }
        }
    }

    async fn plan(&self, _desired: &GarageSpec, _actual: &GarageStatus) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("garage apply: no-op (read-only at v0.0.x)");
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

    fn r(base: &str) -> GarageReconciler<NoOpDriver> {
        GarageReconciler::new(
            GarageEndpoint {
                base_url: base.into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        )
    }

    #[test]
    fn url_join_handles_trailing_slash() {
        assert_eq!(
            r("http://garage:3903/").url("/v1/status").unwrap(),
            "http://garage:3903/v1/status"
        );
    }

    #[test]
    fn url_join_handles_no_trailing_slash() {
        assert_eq!(
            r("http://garage:3903").url("/v1/status").unwrap(),
            "http://garage:3903/v1/status"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        assert!(matches!(
            r("not a url").url("/x"),
            Err(GarageError::InvalidUrl(_))
        ));
    }
}
