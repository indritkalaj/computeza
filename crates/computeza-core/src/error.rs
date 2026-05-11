//! Workspace-wide error type.
//!
//! Every public Computeza API returns [`Result`]. Domain crates wrap their
//! own variants here as they grow; the variants below are placeholders
//! capturing the shape of errors the reconciler loop already needs.

use thiserror::Error;

/// Computeza's top-level error type. Subsystem errors are wrapped here.
#[derive(Debug, Error)]
pub enum Error {
    /// A resource referenced by a spec was not found.
    #[error("resource not found: {0}")]
    NotFound(String),

    /// The actual state observed from a managed component does not match
    /// what the platform's persisted state expects, and the divergence is
    /// not auto-recoverable.
    #[error("unrecoverable drift on {resource}: {detail}")]
    Drift {
        /// The drifted resource's identifier.
        resource: String,
        /// Human-readable detail (NOT user-facing -- i18n happens at the UI layer).
        detail: String,
    },

    /// A reconciler's `apply` step failed and exhausted its retry budget.
    #[error("reconciliation failed for {resource} after {attempts} attempts: {detail}")]
    ReconcileFailed {
        /// Affected resource.
        resource: String,
        /// Number of attempts made.
        attempts: u32,
        /// Underlying detail.
        detail: String,
    },

    /// Driver-level failure (could not start a service, lost connectivity to
    /// a managed component's API, etc.).
    #[error("driver error: {0}")]
    Driver(String),

    /// State-store failure.
    #[error("state error: {0}")]
    State(String),

    /// Catch-all for variants not yet specifically modelled. Prefer adding
    /// a typed variant when a new failure mode appears in real code.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
