//! Computeza state — persistence for the platform's own desired-state metadata.
//!
//! Per spec §3.1 the control plane persists its desired-state metadata in
//! SQLite (single-node deployments) or a dedicated PostgreSQL schema (HA
//! deployments). Per spec §3.5 state is a graph of resources, each with a
//! UUID, kind, name, revision, spec, and status — exactly what this crate
//! stores.
//!
//! v0.0.x ships the SQLite backend. The same trait works against Postgres
//! when HA-mode lands; the SQL is portable apart from a couple of
//! engine-specific bits (UPSERT semantics, partial unique indexes) that
//! we abstract behind the [`Store`] trait.
//!
//! # API shape
//!
//! Callers go through [`Store`]:
//!
//! ```ignore
//! use computeza_state::{SqliteStore, Store, ResourceKey};
//! let store = SqliteStore::open("sqlite:///var/lib/computeza/state.db").await?;
//! let key = ResourceKey::cluster_scoped("postgres-instance", "primary");
//! store.save(&key, &my_spec_json, None).await?;
//! let loaded = store.load(&key).await?;
//! ```
//!
//! `save(.., expected_revision=None)` means "create — fail if the
//! resource already exists". `Some(r)` means "update — fail unless the
//! current revision is exactly `r`". This is how spec §3.5's optimistic
//! concurrency surface gets enforced.

#![warn(missing_docs)]

mod error;
mod sqlite;
mod store;

pub use error::{Result, StateError};
pub use sqlite::SqliteStore;
pub use store::{ResourceKey, Store, StoredResource};
