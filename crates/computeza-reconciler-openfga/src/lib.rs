//! OpenFGA reconciler.
//!
//! Per spec section 7.12 this reconciler manages fine-grained authorization
//! -- authorization model, tuples, namespace assignment. OpenFGA
//! implements the Zanzibar paper better than any Rust alternative as of
//! 2026 (spec section 2.2 "On non-Rust dependencies"); we run it in Go.
//! v0.0.x talks to it over the HTTP playground API (rather than gRPC),
//! snapshotting the store count.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{OpenFgaError, OpenFgaReconciler};
pub use resource::{OpenFgaEndpoint, OpenFgaInstance, OpenFgaSpec, OpenFgaStatus};
