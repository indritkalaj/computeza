//! Kanidm reconciler.
//!
//! Per spec §7.1 this reconciler manages realms, OAuth2 clients, users,
//! groups, password / passkey policies, and IdP federation against a
//! running Kanidm server. v0.0.x is read-only — it observes the server,
//! reports counts and version, and validates the HTTP-reconciler pattern.
//! Write operations (create user / group / OAuth2 client) come in
//! follow-up iterations.
//!
//! # Boundary with the driver
//!
//! Like the Postgres reconciler, this crate assumes Kanidm is already
//! running and reachable. Installing the Kanidm binary, writing the
//! systemd unit, and starting the process is `computeza-driver-native`'s
//! job (spec §7.1 HA story: 3-node read replicas + write replica with
//! manual failover — that's driver territory).

#![warn(missing_docs)]

mod reconciler;
mod resource;

pub use reconciler::{KanidmError, KanidmReconciler};
pub use resource::{KanidmEndpoint, KanidmInstance, KanidmSpec, KanidmStatus};
