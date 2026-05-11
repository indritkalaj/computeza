//! GreptimeDB reconciler.
//!
//! Per spec section 7.10 this reconciler manages unified metrics / logs /
//! traces storage -- storage backend, retention by signal type, OTLP
//! collectors per managed component. Distributed mode (Frontend /
//! Datanode / Metasrv tiers). v0.0.x is read-only: snapshots version +
//! table count via the HTTP admin API.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{GreptimeError, GreptimeReconciler};
pub use resource::{GreptimeEndpoint, GreptimeInstance, GreptimeSpec, GreptimeStatus};
