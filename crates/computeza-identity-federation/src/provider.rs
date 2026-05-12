//! Async trait every IdP binding implements.

use async_trait::async_trait;

use crate::{Claims, IdpConfig, IdpError, IdpKind};

/// Result of the authorization-URL build step. Carries the URL plus
/// the per-flow state value the caller must persist (e.g. in a
/// session cookie) and verify on callback to defeat CSRF on the
/// OIDC dance.
#[derive(Clone, Debug)]
pub struct AuthorizationRequest {
    /// URL the operator's browser must be redirected to. Carries
    /// `client_id`, `redirect_uri`, `response_type=code`, `scope`,
    /// and the binding's PKCE / state parameters.
    pub authorize_url: String,
    /// Opaque state value the caller persists and verifies on
    /// callback. Treat as a secret bound to this auth attempt.
    pub state: String,
    /// PKCE verifier when the binding uses the PKCE flow. `None`
    /// for confidential-client flows.
    pub pkce_verifier: Option<String>,
}

/// Result of the token-exchange step. Carries the verified claims
/// plus the raw tokens for any downstream consumer that wants them.
#[derive(Clone, Debug)]
pub struct TokenResponse {
    /// Normalized claim set from the verified ID token.
    pub claims: Claims,
    /// Raw ID token (JWT) for downstream consumers that want to
    /// re-verify or extract custom claims.
    pub id_token: String,
    /// Access token when the IdP returned one.
    pub access_token: Option<String>,
    /// Refresh token when the binding requested `offline_access`.
    pub refresh_token: Option<String>,
}

/// Async trait every IdP binding implements. v0.0.x stubs every
/// method out to [`IdpError::NotImplemented`]; v0.1 fills in the
/// per-IdP OIDC discovery + token-exchange + JWT verification logic.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// Which IdP kind this provider speaks to. Used by callers
    /// who want to switch on the kind without a downcast.
    fn provider_kind(&self) -> IdpKind;

    /// The config the provider was constructed with. Returned by
    /// reference so callers don't accidentally re-instantiate the
    /// IdP for a single config lookup.
    fn config(&self) -> &IdpConfig;

    /// Build the authorization-URL the operator's browser must be
    /// redirected to. The caller persists [`AuthorizationRequest::state`]
    /// + [`AuthorizationRequest::pkce_verifier`] (in a session or
    /// short-lived cookie) and uses them in
    /// [`Self::exchange_code`].
    async fn build_authorization_request(
        &self,
        scopes: &[&str],
    ) -> Result<AuthorizationRequest, IdpError>;

    /// Exchange the authorization code for tokens, verify the ID
    /// token's signature, and extract normalized claims.
    /// `expected_state` and `expected_pkce_verifier` come from
    /// whatever the caller persisted in
    /// [`Self::build_authorization_request`].
    async fn exchange_code(
        &self,
        code: &str,
        expected_state: &str,
        expected_pkce_verifier: Option<&str>,
    ) -> Result<TokenResponse, IdpError>;
}
