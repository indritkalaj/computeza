//! Normalized claim shape every IdP binding produces. Downstream
//! consumers (login flow, group mapper) read against this; switching
//! on the IdP kind is the binding's job.

use serde::{Deserialize, Serialize};

/// Group claim out of an IdP. The `value` is the raw string from the
/// JWT; the [`crate::ClaimMapping`] rules map it onto a Computeza
/// group name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupClaim {
    /// Which claim path the group came from (`groups`, `roles`, etc.).
    pub claim: String,
    /// Raw value from the IdP.
    pub value: String,
}

/// Normalized post-verification claims. Every concrete
/// [`crate::IdentityProvider`] returns this shape so callers don't
/// have to switch on the IdP kind.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Stable subject identifier from the IdP (the `sub` claim).
    pub subject: String,
    /// Verified email when the IdP supplies one. Some IdPs (AWS IAM,
    /// machine-account flows) don't.
    pub email: Option<String>,
    /// Display name for the operator-facing UI. Falls back to
    /// subject when the IdP doesn't carry a name claim.
    pub display_name: Option<String>,
    /// All group / role claims the IdP returned. The federation
    /// layer's mapping step turns these into local group memberships.
    pub groups: Vec<GroupClaim>,
    /// Issuer URL from the JWT (`iss`).
    pub issuer: String,
    /// Expiry as a Unix timestamp in seconds (`exp`).
    pub expires_at: i64,
}
