//! Concrete IdP bindings.
//!
//! v0.0.x ships real discovery, PKCE generation, and
//! authorization-URL building for every provider; the token-exchange
//! plus JWT-verification step is held back to v0.1+ until we settle
//! on a JWT verifier crate that doesn't drag in OpenSSL (current
//! candidates: jsonwebtoken 10.x with the aws-lc-rs backend, or the
//! openidconnect 4.x crate once its AsyncHttpClient adapter
//! stabilises against reqwest 0.12).
//!
//! `GenericOidcProvider::build_authorization_request` performs real
//! OIDC discovery against the operator-supplied
//! `.well-known/openid-configuration` URL, real PKCE challenge
//! generation (SHA-256), and real authorization-URL assembly. The
//! returned `AuthorizationRequest` carries everything the caller's
//! session needs to persist (state + PKCE verifier) and the URL the
//! operator's browser must hit.
//!
//! Per-IdP wrappers (`EntraIdProvider`, `AwsIamProvider`,
//! `GcpIamProvider`, `KeycloakProvider`) delegate to
//! `GenericOidcProvider` -- the difference between IdPs lives in the
//! operator-supplied discovery URL + claim mappings.
//!
//! `exchange_code` (POST to the token endpoint, ID-token JWT
//! verification against the JWKS, claim normalisation) returns
//! `IdpError::NotImplemented` in v0.0.x. The trait shape is stable
//! so v0.1 fills it in without breaking callers.

use async_trait::async_trait;
use base64ct::{Base64UrlUnpadded, Encoding};
use sha2::{Digest, Sha256};

use crate::{AuthorizationRequest, IdentityProvider, IdpConfig, IdpError, IdpKind, TokenResponse};

/// Real OIDC binding. The other four `IdpKind`s delegate to this.
pub struct GenericOidcProvider {
    config: IdpConfig,
    http: reqwest::Client,
}

/// Per-issuer OIDC discovery document, subset we consume.
/// Everything we don't reference is silently ignored (serde default).
#[derive(Clone, Debug, serde::Deserialize)]
struct DiscoveryDoc {
    /// Issuer URL the IdP self-identifies as.
    #[serde(default)]
    issuer: String,
    /// Authorization endpoint the operator's browser hits.
    authorization_endpoint: String,
    /// Token endpoint we POST the auth-code exchange to (v0.1).
    #[serde(default)]
    #[allow(dead_code)]
    token_endpoint: String,
    /// JWKS URL we fetch to verify ID-token signatures (v0.1).
    #[serde(default)]
    #[allow(dead_code)]
    jwks_uri: String,
}

impl GenericOidcProvider {
    /// Construct from an IdP config. The HTTP client carries
    /// `redirect::Policy::none()` so OIDC redirects don't silently
    /// follow into a foreign issuer -- we want a hard error on
    /// misconfiguration rather than a silent claim swap.
    #[must_use]
    pub fn new(config: IdpConfig) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            // Building a default reqwest client only fails if the
            // system TLS roots are unreadable; panic on construction
            // beats failing per-request.
            .expect("reqwest client build");
        Self { config, http }
    }

    async fn discover(&self) -> Result<DiscoveryDoc, IdpError> {
        let resp = self
            .http
            .get(&self.config.discovery_url)
            .send()
            .await
            .map_err(|e| IdpError::Discovery(format!("GET {}: {e}", self.config.discovery_url)))?;
        if !resp.status().is_success() {
            return Err(IdpError::Discovery(format!(
                "GET {} returned HTTP {}",
                self.config.discovery_url,
                resp.status()
            )));
        }
        resp.json::<DiscoveryDoc>()
            .await
            .map_err(|e| IdpError::Discovery(format!("parsing discovery doc: {e}")))
    }
}

#[async_trait]
impl IdentityProvider for GenericOidcProvider {
    fn provider_kind(&self) -> IdpKind {
        self.config.kind
    }
    fn config(&self) -> &IdpConfig {
        &self.config
    }

    async fn build_authorization_request(
        &self,
        scopes: &[&str],
    ) -> Result<AuthorizationRequest, IdpError> {
        self.config.validate().map_err(IdpError::BadConfig)?;
        let disc = self.discover().await?;
        // Cross-check the issuer claim: many enterprise IdPs serve
        // discovery from a tenant-specific path that must match the
        // `iss` claim. We trust the operator's discovery_url; the
        // server-supplied issuer is logged for diagnostics only.
        tracing_or_noop(&format!(
            "oidc discovery: issuer={} authorization_endpoint={}",
            disc.issuer, disc.authorization_endpoint
        ));

        let (challenge, verifier) = pkce_challenge_s256();
        let state = random_state_token();
        let nonce = random_state_token();

        let scope_str = if scopes.is_empty() {
            "openid".to_string()
        } else {
            // De-dupe `openid` so the wizard adding it twice doesn't
            // produce `scope=openid+openid+email`.
            let mut joined = String::from("openid");
            for s in scopes {
                if *s == "openid" {
                    continue;
                }
                joined.push(' ');
                joined.push_str(s);
            }
            joined
        };
        let mut authorize_url = url::Url::parse(&disc.authorization_endpoint)
            .map_err(|e| IdpError::Discovery(format!("authorization_endpoint not a URL: {e}")))?;
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("scope", &scope_str)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(AuthorizationRequest {
            authorize_url: authorize_url.to_string(),
            state,
            pkce_verifier: Some(verifier),
        })
    }

    async fn exchange_code(
        &self,
        _code: &str,
        expected_state: &str,
        expected_pkce_verifier: Option<&str>,
    ) -> Result<TokenResponse, IdpError> {
        // v0.0.x holds the full token-exchange + JWT-verification
        // flow until we settle on a JWT verifier crate that doesn't
        // pull OpenSSL. Today we still enforce caller hygiene so
        // bugs surface here rather than silently in v0.1.
        if expected_state.is_empty() {
            return Err(IdpError::BadConfig(
                "expected_state must be the state value carried back by the IdP callback".into(),
            ));
        }
        if expected_pkce_verifier.is_none() {
            return Err(IdpError::BadConfig(
                "expected_pkce_verifier is required for the PKCE-secured exchange".into(),
            ));
        }
        Err(IdpError::NotImplemented {
            kind: self.config.kind,
        })
    }
}

/// SHA-256 PKCE challenge generator. Returns `(challenge, verifier)`.
/// The verifier is a 43-128 char URL-safe base64 string per RFC 7636;
/// the challenge is `base64url(sha256(verifier))`.
fn pkce_challenge_s256() -> (String, String) {
    // 32 random bytes -> 43 base64url chars after the `=` is stripped.
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("getrandom failed");
    let verifier = Base64UrlUnpadded::encode_string(&buf);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = Base64UrlUnpadded::encode_string(&hasher.finalize());
    (challenge, verifier)
}

/// 32-byte random token, base64url encoded. Used for `state` + `nonce`.
fn random_state_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("getrandom failed");
    Base64UrlUnpadded::encode_string(&buf)
}

/// Trace through `tracing::debug!` when available; the `tracing` dep
/// is not added to this crate to keep its dep surface bounded, so the
/// helper is a no-op for now. v0.1 wires real tracing once the broader
/// observability story lands.
fn tracing_or_noop(_line: &str) {
    // Intentional no-op for v0.0.x.
}

/// Common scope sets the wizard adds by default for each IdP kind.
/// Operators can extend via the per-component IdP form once that
/// surface gains per-scope inputs (v0.1+). Re-exported at crate root
/// so the UI server can read these without depending on
/// `providers` internals.
#[must_use]
#[allow(dead_code)]
pub fn default_scopes_for(kind: IdpKind) -> &'static [&'static str] {
    match kind {
        IdpKind::EntraId => &["openid", "profile", "email", "User.Read"],
        IdpKind::AwsIam => &["openid", "profile", "email"],
        IdpKind::GcpIam => &["openid", "profile", "email"],
        IdpKind::Keycloak => &["openid", "profile", "email", "roles"],
        IdpKind::GenericOidc => &["openid", "profile", "email"],
    }
}

// ----- Brand-named providers (thin wrappers around GenericOidc) -----

macro_rules! brand_provider {
    ($name:ident, $kind:expr) => {
        #[doc = concat!("Brand-named binding for ", stringify!($name), ". Delegates to GenericOidcProvider.")]
        pub struct $name {
            inner: GenericOidcProvider,
        }
        impl $name {
            /// Construct from an [`IdpConfig`]. The `kind` field
            /// is overwritten to ensure callers don't accidentally
            /// hand the wrong type a config.
            #[must_use]
            pub fn new(mut config: IdpConfig) -> Self {
                config.kind = $kind;
                Self {
                    inner: GenericOidcProvider::new(config),
                }
            }
        }
        #[async_trait]
        impl IdentityProvider for $name {
            fn provider_kind(&self) -> IdpKind {
                $kind
            }
            fn config(&self) -> &IdpConfig {
                self.inner.config()
            }
            async fn build_authorization_request(
                &self,
                scopes: &[&str],
            ) -> Result<AuthorizationRequest, IdpError> {
                self.inner.build_authorization_request(scopes).await
            }
            async fn exchange_code(
                &self,
                code: &str,
                expected_state: &str,
                expected_pkce_verifier: Option<&str>,
            ) -> Result<TokenResponse, IdpError> {
                self.inner
                    .exchange_code(code, expected_state, expected_pkce_verifier)
                    .await
            }
        }
    };
}

brand_provider!(EntraIdProvider, IdpKind::EntraId);
brand_provider!(AwsIamProvider, IdpKind::AwsIam);
brand_provider!(GcpIamProvider, IdpKind::GcpIam);
brand_provider!(KeycloakProvider, IdpKind::Keycloak);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider_for_config, ClaimMapping};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_config(kind: IdpKind, base_url: &str) -> IdpConfig {
        IdpConfig {
            kind,
            discovery_url: format!("{base_url}/.well-known/openid-configuration"),
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

    #[test]
    fn default_scopes_have_openid_for_every_kind() {
        for k in IdpKind::all() {
            let scopes = default_scopes_for(*k);
            assert!(
                scopes.contains(&"openid"),
                "{k:?} default scopes must include openid"
            );
        }
    }

    #[test]
    fn provider_for_config_returns_matching_kind() {
        for kind in IdpKind::all() {
            let provider = provider_for_config(sample_config(*kind, "https://example.com"));
            assert_eq!(provider.provider_kind(), *kind);
        }
    }

    #[test]
    fn pkce_challenge_is_43_chars_and_deterministic() {
        let (challenge, verifier) = pkce_challenge_s256();
        // base64url of 32 bytes => 43 chars (no padding).
        assert_eq!(challenge.len(), 43);
        assert_eq!(verifier.len(), 43);
        // Verifier should hash to the challenge -- enforce SHA-256
        // explicitly so a future swap to a different hash gets caught.
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let recomputed = Base64UrlUnpadded::encode_string(&hasher.finalize());
        assert_eq!(challenge, recomputed);
    }

    #[test]
    fn pkce_challenges_differ_across_calls() {
        let (c1, v1) = pkce_challenge_s256();
        let (c2, v2) = pkce_challenge_s256();
        // CSPRNG output; collision is cryptographically negligible.
        assert_ne!(c1, c2);
        assert_ne!(v1, v2);
    }

    #[test]
    fn random_state_token_is_43_chars_and_unique() {
        let a = random_state_token();
        let b = random_state_token();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    /// End-to-end auth-URL build against a wiremock-served discovery
    /// doc. Asserts every required OIDC parameter is present, the
    /// scope set has been de-duped, and PKCE shape is correct.
    #[tokio::test]
    async fn build_authorization_request_against_mock_issuer() {
        let server = MockServer::start().await;
        let discovery_body = serde_json::json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/auth", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "jwks_uri": format!("{}/jwks", server.uri()),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_body))
            .mount(&server)
            .await;

        let provider = GenericOidcProvider::new(sample_config(IdpKind::GenericOidc, &server.uri()));
        let auth = provider
            .build_authorization_request(&["openid", "email", "openid"])
            .await
            .expect("build_authorization_request");

        let url = url::Url::parse(&auth.authorize_url).expect("authorize_url parses");
        assert_eq!(url.host_str().unwrap(), "127.0.0.1");
        let params: std::collections::HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("computeza")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("https://console.example.com/auth/callback")
        );
        // `openid` deduped + email kept.
        let scope = params.get("scope").expect("scope present");
        assert_eq!(
            scope.split(' ').filter(|s| *s == "openid").count(),
            1,
            "openid scope must be present exactly once even when the caller repeats it"
        );
        assert!(scope.contains("email"));
        assert!(params.contains_key("state"));
        assert!(params.contains_key("nonce"));
        let challenge = params.get("code_challenge").expect("challenge");
        assert_eq!(challenge.len(), 43);
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );

        assert_eq!(auth.pkce_verifier.as_deref().unwrap().len(), 43);
        assert!(auth.state.len() >= 16);
    }

    #[tokio::test]
    async fn build_authorization_request_surfaces_discovery_5xx_as_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let provider = GenericOidcProvider::new(sample_config(IdpKind::GenericOidc, &server.uri()));
        let err = provider
            .build_authorization_request(&["openid"])
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::Discovery(_)));
    }

    #[tokio::test]
    async fn build_authorization_request_rejects_malformed_discovery_doc() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let provider = GenericOidcProvider::new(sample_config(IdpKind::GenericOidc, &server.uri()));
        let err = provider
            .build_authorization_request(&["openid"])
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::Discovery(_)));
    }

    #[tokio::test]
    async fn build_authorization_request_rejects_bad_authorization_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": server.uri(),
                "authorization_endpoint": "not-a-url",
                "token_endpoint": "https://x",
                "jwks_uri": "https://y",
            })))
            .mount(&server)
            .await;
        let provider = GenericOidcProvider::new(sample_config(IdpKind::GenericOidc, &server.uri()));
        let err = provider
            .build_authorization_request(&["openid"])
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::Discovery(_)));
    }

    #[tokio::test]
    async fn build_authorization_request_validates_config_first() {
        let mut config = sample_config(IdpKind::GenericOidc, "https://x");
        config.client_id = String::new();
        let provider = GenericOidcProvider::new(config);
        let err = provider
            .build_authorization_request(&["openid"])
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::BadConfig(_)));
    }

    #[tokio::test]
    async fn exchange_code_returns_not_implemented_but_validates_inputs_first() {
        let provider = GenericOidcProvider::new(sample_config(IdpKind::GenericOidc, "https://x"));
        // Empty state -> BadConfig (tripwire).
        let err = provider
            .exchange_code("c", "", Some("v"))
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::BadConfig(_)));
        // Missing verifier -> BadConfig.
        let err = provider.exchange_code("c", "s", None).await.unwrap_err();
        assert!(matches!(err, IdpError::BadConfig(_)));
        // All inputs valid -> NotImplemented (v0.1 work).
        let err = provider
            .exchange_code("c", "s", Some("v"))
            .await
            .unwrap_err();
        assert!(matches!(err, IdpError::NotImplemented { .. }));
    }

    #[tokio::test]
    async fn brand_provider_overwrites_kind_in_config() {
        // Construct EntraId with a config that accidentally claims
        // GenericOidc kind; the wrapper must overwrite to EntraId.
        let config = sample_config(IdpKind::GenericOidc, "https://x");
        let provider = EntraIdProvider::new(config);
        assert_eq!(provider.provider_kind(), IdpKind::EntraId);
        assert_eq!(provider.config().kind, IdpKind::EntraId);
    }
}
