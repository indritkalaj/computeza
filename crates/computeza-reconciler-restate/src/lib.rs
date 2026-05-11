//! Restate reconciler.
//!
//! Per spec section 7.9 this reconciler manages the durable execution
//! orchestrator -- service registrations, deployment mode, retention,
//! journal storage. Restate is also the runtime for compiled pipelines
//! (spec section 5.2) and for agentic workflows in the AI Workspace
//! (spec section 6.5). v0.0.x is read-only: snapshots service count
//! and deployment count via the admin HTTP API.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{RestateError, RestateReconciler};
pub use resource::{RestateEndpoint, RestateInstance, RestateSpec, RestateStatus};
