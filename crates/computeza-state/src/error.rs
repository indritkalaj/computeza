//! State-store errors.

use thiserror::Error;

/// Errors emitted by the state store.
#[derive(Debug, Error)]
pub enum StateError {
    /// SQLx-level failure.
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// JSON (de)serialization failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Optimistic-concurrency check failed: the caller's `expected_revision`
    /// did not match what's in the store.
    #[error("revision conflict on {key}: expected {expected:?}, found {found:?}")]
    RevisionConflict {
        /// Human-readable key (kind/name[/workspace]).
        key: String,
        /// What the caller passed as `expected_revision`.
        expected: Option<u64>,
        /// What the store actually has.
        found: Option<u64>,
    },
    /// Caller tried to create a resource that already exists, or update
    /// one that doesn't.
    #[error("resource {0} not found")]
    NotFound(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, StateError>;
