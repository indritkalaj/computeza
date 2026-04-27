//! Computeza core — domain types, declarative state schema, foundational traits.
//!
//! This crate is the dependency root of the workspace. Every other Computeza
//! crate depends on it and nothing here depends on anything Computeza-specific.
//! Keep it minimal: only types and traits that are genuinely cross-cutting.
//!
//! See spec §3 for the architecture this crate underpins.

#![warn(missing_docs)]

pub mod driver;
pub mod error;
pub mod health;
pub mod reconciler;
pub mod resource;

pub use driver::{Driver, NoOpDriver};
pub use error::{Error, Result};
pub use health::Health;
pub use reconciler::Reconciler;
pub use resource::Resource;
