//! `QdrantReconciler` -- read-only HTTP reconciler.

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

use crate::resource::{QdrantEndpoint, QdrantInstance, QdrantSpec, QdrantStatus};

/// Qdrant-reconciler-specific errors.
#[derive(Debug, Error)]
pub enum QdrantError {
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
    #[error("qdrant responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<QdrantError> for CoreError {
    fn from(e: QdrantError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Qdrant cluster.
pub struct QdrantReconciler<D: Driver = NoOpDriver> {
    endpoint: QdrantEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

impl<D: Driver> QdrantReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and admin token.
    pub fn new(endpoint: QdrantEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            _driver: std::marker::PhantomData,
        }
    }

    fn build_client(insecure: bool) -> Result<reqwest::Client, QdrantError> {
        if insecure {
            warn!("qdrant: insecure_skip_tls_verify=true; TLS validation disabled");
        }
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-qdrant/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, QdrantError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, QdrantError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| QdrantError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| QdrantError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, QdrantError> {
        let url = self.url(path)?;
        let token = self.admin_token.expose_secret();
        let resp = self
            .http()
            .await?
            .get(&url)
            .header("api-key", token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(QdrantError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<QdrantStatus, QdrantError> {
        // Qdrant exposes `/` for version metadata and `/collections` for
        // the collection list (under `result.collections`).
        let root = self.get_json("/").await.ok();
        let server_version = root
            .as_ref()
            .and_then(|v| v.get("version"))
            .and_then(|x| x.as_str())
            .map(str::to_string);

        let collection_count = match self.get_json("/collections").await {
            Ok(v) => v
                .pointer("/result/collections")
                .and_then(|c| c.as_array())
                .map(|a| a.len() as u64),
            Err(_) => None,
        };

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(QdrantStatus {
            server_version,
            collection_count,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for QdrantReconciler<D> {
    type Resource = QdrantInstance;
    type Driver = D;
    type Plan = ();

    async fn observe(&self, _ctx: &Context) -> Result<QdrantStatus, CoreError> {
        match self.snapshot().await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(error = %e, "qdrant observe failed");
                Ok(QdrantStatus {
                    last_observe_failed: true,
                    ..QdrantStatus::default()
                })
            }
        }
    }

    async fn plan(&self, _desired: &QdrantSpec, _actual: &QdrantStatus) -> Result<(), CoreError> {
        Ok(())
    }

    async fn apply(&self, _ctx: &Context, _plan: (), _driver: &D) -> Result<Outcome, CoreError> {
        debug!("qdrant apply: no-op (read-only at v0.0.x)");
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

    fn r(base: &str) -> QdrantReconciler<NoOpDriver> {
        QdrantReconciler::new(
            QdrantEndpoint {
                base_url: base.into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        )
    }

    #[test]
    fn url_join_works() {
        assert_eq!(
            r("http://qdrant:6333").url("/collections").unwrap(),
            "http://qdrant:6333/collections"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        assert!(matches!(
            r("not a url").url("/x"),
            Err(QdrantError::InvalidUrl(_))
        ));
    }
}
