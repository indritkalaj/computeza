//! Health snapshot reported by reconcilers and drivers.

use serde::{Deserialize, Serialize};

/// Health of a managed resource at a point in time.
///
/// Reconcilers refresh health every 30 seconds (spec section 3.3); drift indicators
/// in the UI render this directly. The `Healthy` / `Degraded` / `Unhealthy`
/// triad maps to spec section 4.4's green / amber / red drift colours.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Desired and actual state agree.
    Healthy,
    /// Reconciliation is in progress, or the component is degraded but
    /// auto-recoverable.
    Degraded {
        /// Machine-readable detail; UI renders an i18n key derived from this.
        reason: String,
    },
    /// Reconciliation has failed and is not auto-recovering.
    Unhealthy {
        /// Machine-readable detail.
        reason: String,
    },
    /// Status has not yet been observed (initial state).
    Unknown,
}

impl Health {
    /// Convenience constructor for the common `Healthy` case.
    #[must_use]
    pub fn ok() -> Self {
        Self::Healthy
    }
}
