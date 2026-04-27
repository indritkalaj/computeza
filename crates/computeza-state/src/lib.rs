//! Computeza state — persistence layer for the platform's own metadata.
//!
//! Per spec §3.1, the control plane persists its desired-state metadata in
//! SQLite (single-node deployments) or a dedicated PostgreSQL schema (HA
//! deployments). This crate owns the schema, migrations, and SQLx-based
//! repository implementations.
//!
//! Scaffold stub. Implementation is pending; see spec §3.5 for the state
//! model that this crate will materialise.

#![warn(missing_docs)]
