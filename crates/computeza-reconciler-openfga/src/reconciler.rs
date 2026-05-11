//! `OpenFgaReconciler` -- read-only HTTP reconciler.

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

use crate::resource::{OpenFgaEndpoint, OpenFgaInstance, OpenFgaSpec, OpenFgaStatus};

/// OpenFGA-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum OpenFgaError {
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
    #[error("openfga responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<OpenFgaError> for CoreError {
    fn from(e: OpenFgaError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one OpenFGA instance.
pub struct OpenFgaReconciler<D: Driver = NoOpDriver> {
    endpoint: OpenFgaEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

impl<D: Driver> OpenFgaReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and admin token.
    pub fn new(endpoint: OpenFgaEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            _driver: std::marker::PhantomData,
        }
    }

    fn build_client(insecure: bool) -> Result<reqwest::Client, OpenFgaError> {
        if insecure {
            warn!("openfga: insecure_skip_tls_verify=true; TLS validation disabled");
        }
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-openfga/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, OpenFgaError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, OpenFgaError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| OpenFgaError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| OpenFgaError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, OpenFgaError> {
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
            return Err(OpenFgaError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<OpenFgaStatus, OpenFgaError> {
        // OpenFGA exposes /healthz for liveness and /stores for the store
        // list (response body has a `stores` array).
        let _health = self.get_json("/healthz").await.ok();

        let store_count = match self.get_json("/stores").await {
            Ok(v) => v
                .get("stores")
                .and_then(|s| s.as_array())
                .map(|a| a.len() as u64),
            Err(_) => None,
        };

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(OpenFgaStatus {
            // /healthz on OpenFGA doesn't expose the build version reliably;
            // leave server_version None until the metadata endpoint stabilises.
            server_version: None,
            store_count,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for OpenFgaReconciler<D> {
    type Resource = OpenFgaInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<OpenFgaStatus, CoreError> {
        match self.snapshot().await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(error = %e, "openfga observe failed");
                Ok(OpenFgaStatus {
                    last_observe_failed: true,
                    ..OpenFgaStatus::default()
                })
            }
        }
    }

    async fn plan(&self, _desired: &OpenFgaSpec, _actual: &OpenFgaStatus) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("openfga apply: no-op (read-only at v0.0.x)");
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

    fn r(base: &str) -> OpenFgaReconciler<NoOpDriver> {
        OpenFgaReconciler::new(
            OpenFgaEndpoint {
                base_url: base.into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        )
    }

    #[test]
    fn url_join_works() {
        assert_eq!(
            r("http://openfga:8080").url("/stores").unwrap(),
            "http://openfga:8080/stores"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        assert!(matches!(
            r("not a url").url("/x"),
            Err(OpenFgaError::InvalidUrl(_))
        ));
    }
}
