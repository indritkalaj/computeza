//! Qdrant reconciler.
//!
//! Per spec section 7.8 this reconciler manages the production RAG retrieval
//! API -- collections, JWT RBAC federated with Kanidm, snapshot policies,
//! sharding. v0.0.x is read-only: snapshots the running cluster's version
//! and collection count via the admin HTTP API.

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{QdrantError, QdrantReconciler};
pub use resource::{QdrantEndpoint, QdrantInstance, QdrantSpec, QdrantStatus};
