//! Computeza driver registry.
//!
//! Per spec §3.2, drivers abstract the deployment target — native (v1.0),
//! Kubernetes (v1.2), AWS / Azure / GCP (v1.2). This crate hosts the
//! registry that the platform queries to obtain a `Driver` impl by target,
//! and re-exports the [`computeza_core::Driver`] trait for convenience.
//!
//! New drivers can be registered by partners via dynamically-loaded
//! libraries (spec §3.4 engineering note). The registration mechanism
//! lands here when the driver-loading machinery is built.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]

pub use computeza_core::Driver;
