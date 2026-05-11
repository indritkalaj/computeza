//! Garage reconciler.
//!
//! Per spec section 7.2 this reconciler manages the geo-distributed S3-compatible
//! Garage cluster -- cluster layout, zones, replication, buckets, access
//! keys, lifecycle. v0.0.x is read-only: snapshots `/v1/status` and
//! `/v1/bucket` so the operator console can render cluster topology and
//! bucket counts.
//!
//! Write operations (bucket create / delete, access-key issuance, layout
//! application) ship in follow-ups. The HTTP-reconciler pattern is the
//! same one [`computeza-reconciler-kanidm`] uses.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{GarageError, GarageReconciler};
pub use resource::{GarageEndpoint, GarageInstance, GarageSpec, GarageStatus};
