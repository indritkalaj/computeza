//! Sail reconciler.
//!
//! Manages the LakeSail Sail Spark-API engine. v0.0.x is read-only:
//! probes the Spark Connect gRPC port (TCP-level) to confirm the
//! daemon is reachable. A richer protocol-level health check would
//! require pulling in the Spark Connect proto definitions; for v0.0.x
//! "is the port accepting TCP connections" is a useful liveness signal
//! that costs ~50 lines instead of ~500.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{SailError, SailReconciler};
pub use resource::{SailEndpoint, SailInstance, SailSpec, SailStatus};
