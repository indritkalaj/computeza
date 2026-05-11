//! Databend reconciler.
//!
//! Per spec section 7.6 this reconciler manages the columnar SQL engine
//! (vector + FTS + geospatial) -- cluster topology, query node count,
//! catalog wiring, users. v0.0.x is read-only: snapshots server version
//! and node count via the admin HTTP API.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{DatabendError, DatabendReconciler};
pub use resource::{DatabendEndpoint, DatabendInstance, DatabendSpec, DatabendStatus};
