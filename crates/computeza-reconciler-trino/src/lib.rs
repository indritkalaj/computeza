//! Trino reconciler.
//!
//! Manages the Trino SQL engine. v0.0.x is read-only: probes the
//! HTTP port (TCP-level) to confirm the coordinator is reachable.
//! A protocol-level health check would post a trivial query to
//! `/v1/statement`; for v0.0.x the TCP probe is enough -- the
//! Studio editor exercises end-to-end protocol round-trips on
//! every operator query.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{TrinoError, TrinoReconciler};
pub use resource::{TrinoEndpoint, TrinoInstance, TrinoSpec, TrinoStatus};
