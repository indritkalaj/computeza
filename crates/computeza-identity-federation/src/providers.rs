//! Per-IdP concrete provider scaffolds. Every method short-circuits
//! to [`IdpError::NotImplemented`] in v0.0.x; v0.1 fills in the
//! OIDC discovery + token-exchange + verification logic per-IdP.
//!
//! The shape is stable: when v0.1 lands these stubs become real
//! without changing any caller code.

use async_trait::async_trait;

use crate::{AuthorizationRequest, IdentityProvider, IdpConfig, IdpError, IdpKind, TokenResponse};

macro_rules! stub_provider {
    ($name:ident, $kind:expr) => {
        #[doc = concat!("Stub binding for ", stringify!($name), ". v0.1+ implements the OIDC flow.")]
        pub struct $name {
            config: IdpConfig,
        }

        impl $name {
            /// Construct from an [`IdpConfig`] whose `kind` matches.
            #[must_use]
            pub fn new(config: IdpConfig) -> Self {
                Self { config }
            }
        }

        #[async_trait]
        impl IdentityProvider for $name {
            fn provider_kind(&self) -> IdpKind {
                $kind
            }
            fn config(&self) -> &IdpConfig {
                &self.config
            }
            async fn build_authorization_request(
                &self,
                _scopes: &[&str],
            ) -> Result<AuthorizationRequest, IdpError> {
                Err(IdpError::NotImplemented { kind: $kind })
            }
            async fn exchange_code(
                &self,
                _code: &str,
                _expected_state: &str,
                _expected_pkce_verifier: Option<&str>,
            ) -> Result<TokenResponse, IdpError> {
                Err(IdpError::NotImplemented { kind: $kind })
            }
        }
    };
}

stub_provider!(EntraIdProvider, IdpKind::EntraId);
stub_provider!(AwsIamProvider, IdpKind::AwsIam);
stub_provider!(GcpIamProvider, IdpKind::GcpIam);
stub_provider!(KeycloakProvider, IdpKind::Keycloak);
stub_provider!(GenericOidcProvider, IdpKind::GenericOidc);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider_for_config, ClaimMapping};

    fn sample_config(kind: IdpKind) -> IdpConfig {
        IdpConfig {
            kind,
            discovery_url: "https://example.com/.well-known/openid-configuration".into(),
            client_id: "computeza".into(),
            client_secret_ref: None,
            redirect_uri: "https://console.example.com/auth/callback".into(),
            claim_mappings: vec![ClaimMapping {
                claim: "groups".into(),
                value: "computeza-admins".into(),
                local_group: "admins".into(),
            }],
        }
    }

    #[tokio::test]
    async fn every_provider_returns_not_implemented_in_v00x() {
        for kind in IdpKind::all() {
            let provider = provider_for_config(sample_config(*kind));
            assert_eq!(provider.provider_kind(), *kind);
            let err = provider
                .build_authorization_request(&["openid", "email"])
                .await
                .unwrap_err();
            assert!(matches!(err, IdpError::NotImplemented { .. }));
            let err = provider
                .exchange_code("dummy-code", "dummy-state", None)
                .await
                .unwrap_err();
            assert!(matches!(err, IdpError::NotImplemented { .. }));
        }
    }
}
