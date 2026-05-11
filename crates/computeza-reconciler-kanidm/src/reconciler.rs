//! `KanidmReconciler` — implements [`computeza_core::Reconciler`] against a
//! running Kanidm server.
//!
//! v0.0.x is read-only: `observe()` snapshots server version + group/person
//! counts; `plan()` always produces an empty plan; `apply()` is a no-op.
//! This proves the HTTP-reconciler pattern (mirroring the SQL-reconciler
//! pattern in `computeza-reconciler-postgres`) without committing to a
//! specific user/group/OAuth2 management API ahead of need.

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

use crate::resource::{KanidmEndpoint, KanidmInstance, KanidmSpec, KanidmStatus};

/// Kanidm-reconciler-specific errors. Wrapped into
/// [`computeza_core::Error`] before crossing the trait boundary.
#[derive(Debug, Error)]
pub enum KanidmError {
    /// HTTP transport failure.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON decode failure on a Kanidm response.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// The endpoint URL is malformed.
    #[error("invalid base_url: {0}")]
    InvalidUrl(String),

    /// The server returned a non-2xx status.
    #[error("kanidm responded with status {status} for {path}")]
    UnexpectedStatus {
        /// HTTP status returned.
        status: u16,
        /// Request path that failed.
        path: String,
    },
}

impl From<KanidmError> for CoreError {
    fn from(e: KanidmError) -> Self {
        CoreError::Other(anyhow::Error::new(e))
    }
}

/// Read-only reconciler for one Kanidm server instance.
pub struct KanidmReconciler<D: Driver = NoOpDriver> {
    endpoint: KanidmEndpoint,
    admin_token: SecretString,
    client: OnceCell<reqwest::Client>,
    last_observed: Mutex<Option<chrono::DateTime<Utc>>>,
    _driver: std::marker::PhantomData<Arc<D>>,
}

impl<D: Driver> KanidmReconciler<D> {
    /// Construct a reconciler bound to the given endpoint and admin token.
    /// The HTTP client is built lazily on first use.
    pub fn new(endpoint: KanidmEndpoint, admin_token: SecretString) -> Self {
        Self {
            endpoint,
            admin_token,
            client: OnceCell::new(),
            last_observed: Mutex::new(None),
            _driver: std::marker::PhantomData,
        }
    }

    fn build_client(insecure: bool) -> Result<reqwest::Client, KanidmError> {
        Ok(reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!(
                "computeza-reconciler-kanidm/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?)
    }

    async fn http(&self) -> Result<&reqwest::Client, KanidmError> {
        self.client
            .get_or_try_init(|| async {
                Self::build_client(self.endpoint.insecure_skip_tls_verify)
            })
            .await
    }

    fn url(&self, path: &str) -> Result<String, KanidmError> {
        let base = url::Url::parse(&self.endpoint.base_url)
            .map_err(|_| KanidmError::InvalidUrl(self.endpoint.base_url.clone()))?;
        base.join(path)
            .map(|u| u.to_string())
            .map_err(|_| KanidmError::InvalidUrl(format!("{base}{path}")))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, KanidmError> {
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
            return Err(KanidmError::UnexpectedStatus {
                status: status.as_u16(),
                path: path.into(),
            });
        }
        Ok(resp.json::<serde_json::Value>().await?)
    }

    async fn snapshot(&self) -> Result<KanidmStatus, KanidmError> {
        // Kanidm exposes `/status` (unauthenticated liveness) and a versioned
        // REST under `/v1/`. The exact shape of `/v1/system` and the group /
        // person endpoints is somewhat fluid across Kanidm releases, so we
        // tolerate variations: anything that returns a JSON array gets its
        // length counted, anything else stays None.
        let version = match self.get_json("/v1/system").await {
            Ok(v) => v
                .get("version")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            Err(KanidmError::UnexpectedStatus { status: 404, .. }) => None,
            Err(e) => return Err(e),
        };

        let group_count = self.count_collection("/v1/group").await.ok();
        let person_count = self.count_collection("/v1/person").await.ok();

        let now = Utc::now();
        *self.last_observed.lock().await = Some(now);

        Ok(KanidmStatus {
            server_version: version,
            group_count,
            person_count,
            last_observed_at: Some(now),
            last_observe_failed: false,
        })
    }

    async fn count_collection(&self, path: &str) -> Result<u64, KanidmError> {
        let v = self.get_json(path).await?;
        let n = v.as_array().map(|a| a.len()).unwrap_or(0);
        Ok(n as u64)
    }
}

#[async_trait]
impl<D: Driver + 'static> Reconciler for KanidmReconciler<D> {
    type Resource = KanidmInstance;
    type Driver = D;
    type Plan = (); // Read-only at v0.0.x — no managed state to converge.

    async fn observe(&self, _ctx: &Context) -> Result<KanidmStatus, CoreError> {
        match self.snapshot().await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(error = %e, "kanidm observe failed");
                Ok(KanidmStatus {
                    last_observe_failed: true,
                    ..KanidmStatus::default()
                })
            }
        }
    }

    async fn plan(
        &self,
        _desired: &KanidmSpec,
        _actual: &KanidmStatus,
    ) -> Result<(), CoreError> {
        // No managed state yet — plan is always empty.
        Ok(())
    }

    async fn apply(
        &self,
        _ctx: &Context,
        _plan: (),
        _driver: &D,
    ) -> Result<Outcome, CoreError> {
        debug!("kanidm apply: no-op (read-only at v0.0.x)");
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

    #[test]
    fn url_join_handles_trailing_slash() {
        let r: KanidmReconciler<NoOpDriver> = KanidmReconciler::new(
            KanidmEndpoint {
                base_url: "https://idm.example.com/".into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        );
        assert_eq!(
            r.url("/v1/group").unwrap(),
            "https://idm.example.com/v1/group"
        );
    }

    #[test]
    fn url_join_handles_no_trailing_slash() {
        let r: KanidmReconciler<NoOpDriver> = KanidmReconciler::new(
            KanidmEndpoint {
                base_url: "https://idm.example.com".into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        );
        assert_eq!(
            r.url("/v1/group").unwrap(),
            "https://idm.example.com/v1/group"
        );
    }

    #[test]
    fn url_join_rejects_invalid_base() {
        let r: KanidmReconciler<NoOpDriver> = KanidmReconciler::new(
            KanidmEndpoint {
                base_url: "not a url".into(),
                insecure_skip_tls_verify: false,
            },
            SecretString::from("token"),
        );
        assert!(matches!(r.url("/x"), Err(KanidmError::InvalidUrl(_))));
    }
}
