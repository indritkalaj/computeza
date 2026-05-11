//! Lakekeeper reconciler.
//!
//! Per spec section 7.4 this reconciler manages the Iceberg REST catalog with
//! Generic Tables support -- projects, warehouses, OIDC config, OpenFGA
//! model deployment, vended credentials, Generic Tables for Delta and
//! Lance. v0.0.x is read-only: snapshots server version + warehouse
//! count + project count.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{LakekeeperError, LakekeeperReconciler};
pub use resource::{LakekeeperEndpoint, LakekeeperInstance, LakekeeperSpec, LakekeeperStatus};
