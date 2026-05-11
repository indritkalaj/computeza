//! PostgreSQL reconciler.
//!
//! Per spec section 7.13, this reconciler manages database creation, migration
//! sequencing, and (eventually) backup orchestration against a running
//! PostgreSQL instance. It communicates over libpq via SQLx.
//!
//! # Boundary with the driver
//!
//! The driver (`computeza-driver-native`) is responsible for *installing*
//! and *running* the PostgreSQL server: laying down the binary, writing the
//! systemd unit / launchd plist / Windows Service registration, starting
//! the process, exposing the port. This reconciler assumes the server is
//! already running and focuses on the *configuration* layer above it --
//! creating databases, managing users, applying schema migrations, and
//! observing replication health.
//!
//! This separation matches how every credible Postgres operator (Patroni,
//! Zalando's postgres-operator, CloudNative-PG) is structured. Mixing the
//! two responsibilities is a recurring source of bugs.
//!
//! # What v0.0.2 ships
//!
//! - [`PostgresInstance`] resource type with [`PostgresSpec`] / [`PostgresStatus`]
//! - [`PostgresReconciler`] implementing [`computeza_core::Reconciler`]:
//!   - `observe()` connects via SQLx and snapshots `pg_database` plus version
//!   - `plan()` diffs the desired vs actual database list
//!   - `apply()` executes `CREATE DATABASE` / `DROP DATABASE` per the plan
//!   - `health()` returns Healthy when the most recent observe succeeded
//! - Unit tests for plan computation (no network required)
//! - An `#[ignore]`-gated integration test that needs a real Postgres
//!
//! # Not yet
//!
//! - User and role management (next iteration)
//! - Schema migrations (depends on a chosen migration runner)
//! - Streaming replication / Patroni HA (spec section 7.13)
//! - Backup orchestration

#![warn(missing_docs)]

mod plan;
mod reconciler;
mod resource;

pub use plan::{DatabaseChange, PostgresPlan};
pub use reconciler::PostgresReconciler;
pub use resource::{DatabaseSpec, PostgresInstance, PostgresSpec, PostgresStatus, ServerEndpoint};
