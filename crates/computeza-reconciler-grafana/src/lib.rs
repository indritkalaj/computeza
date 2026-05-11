//! Grafana reconciler.
//!
//! Per spec section 7.11 this reconciler manages BI and visualisation
//! dashboards -- datasources auto-wired to GreptimeDB and Databend,
//! dashboards-as-code, OIDC. Grafana is the only non-Rust UI component;
//! the spec notes we'll revisit when a production-grade Rust BI
//! replacement matures. v0.0.x is read-only: snapshots version, folder
//! count, dashboard count via the HTTP admin API.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{GrafanaError, GrafanaReconciler};
pub use resource::{GrafanaEndpoint, GrafanaInstance, GrafanaSpec, GrafanaStatus};
