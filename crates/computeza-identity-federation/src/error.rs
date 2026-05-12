//! Error type surface for the federation layer.

/// Errors returned by every [`crate::IdentityProvider`] method.
#[derive(Debug, thiserror::Error)]
pub enum IdpError {
    /// The IdP binding's logic has not yet been implemented. Returned
    /// by every concrete provider in v0.0.x so callers learn the
    /// shape but don't accidentally rely on real verification.
    #[error("identity provider not yet implemented for {kind:?} (v0.1+ feature)")]
    NotImplemented {
        /// Which IdP kind the caller tried to invoke.
        kind: crate::IdpKind,
    },
    /// The OIDC discovery document at `discovery_url` could not be
    /// fetched or parsed.
    #[error("discovery: {0}")]
    Discovery(String),
    /// The authorization-code exchange failed -- typically because
    /// the code expired or didn't match the registered client.
    #[error("token exchange: {0}")]
    TokenExchange(String),
    /// The ID token signature or claim set failed verification.
    #[error("token verification: {0}")]
    TokenVerification(String),
    /// The IdP config is structurally invalid (e.g. malformed
    /// discovery URL, missing client_id).
    #[error("bad config: {0}")]
    BadConfig(String),
    /// I/O during the OIDC dance failed (network, file system).
    #[error("io: {0}")]
    Io(String),
}
