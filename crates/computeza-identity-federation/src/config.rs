//! Per-tenant IdP configuration. Serializable so the operator
//! console can persist it alongside install configs in the metadata
//! store; the actual `client_secret` value lives in
//! [`computeza-secrets`] referenced by `secret_ref`.

use serde::{Deserialize, Serialize};

/// IdP flavors Computeza federates against. Closed enum -- adding a
/// new flavor means adding a new provider implementation in
/// [`crate::providers`] and a new variant here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdpKind {
    /// Microsoft Entra ID (formerly Azure AD).
    EntraId,
    /// AWS IAM Identity Center (formerly AWS SSO).
    AwsIam,
    /// GCP Identity Platform (Cloud Identity-Aware Proxy + IAM).
    GcpIam,
    /// Keycloak / Red Hat SSO (also used as the upstream IdP
    /// stand-in for CI integration tests).
    Keycloak,
    /// Any other OIDC-compliant provider. Treated as the "I have a
    /// discovery URL and a client_id, figure it out" path.
    GenericOidc,
}

impl IdpKind {
    /// Human-readable label for the operator-facing form dropdown.
    /// Not localized -- IdP brand names cross language boundaries
    /// unchanged.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            IdpKind::EntraId => "Microsoft Entra ID",
            IdpKind::AwsIam => "AWS IAM Identity Center",
            IdpKind::GcpIam => "GCP Identity Platform",
            IdpKind::Keycloak => "Keycloak",
            IdpKind::GenericOidc => "Generic OIDC",
        }
    }

    /// Stable string form used in form value attributes + JSON
    /// serialization.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            IdpKind::EntraId => "entra-id",
            IdpKind::AwsIam => "aws-iam",
            IdpKind::GcpIam => "gcp-iam",
            IdpKind::Keycloak => "keycloak",
            IdpKind::GenericOidc => "generic-oidc",
        }
    }

    /// Every IdP kind we ship a stub for. Useful for rendering the
    /// per-component form dropdown.
    #[must_use]
    pub const fn all() -> &'static [IdpKind] {
        &[
            IdpKind::EntraId,
            IdpKind::AwsIam,
            IdpKind::GcpIam,
            IdpKind::Keycloak,
            IdpKind::GenericOidc,
        ]
    }
}

/// Per-tenant binding to an upstream IdP. Persisted alongside the
/// component install-config so each managed component can federate
/// against its own provider (or share one across the install).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdpConfig {
    /// Which IdP flavor this config targets.
    pub kind: IdpKind,
    /// OIDC discovery URL. For Entra ID this is
    /// `https://login.microsoftonline.com/<tenant>/v2.0/.well-known/openid-configuration`;
    /// for Keycloak it's
    /// `https://<host>/realms/<realm>/.well-known/openid-configuration`.
    pub discovery_url: String,
    /// OAuth2 client ID registered at the IdP for this Computeza
    /// install.
    pub client_id: String,
    /// Optional opaque reference into the [`computeza-secrets`]
    /// store carrying the client secret. `None` for public clients
    /// (mobile-shaped PKCE flows) or for self-hosted Keycloak with
    /// confidential-client disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_ref: Option<String>,
    /// Redirect URI registered at the IdP for this Computeza install.
    /// Typically `https://<your-console>/auth/callback`.
    pub redirect_uri: String,
    /// How to map IdP claims onto local Computeza groups. Mandatory
    /// because without it a federated session has no permissions.
    #[serde(default)]
    pub claim_mappings: Vec<ClaimMapping>,
}

/// Rule that maps a claim value out of the IdP onto a Computeza
/// group name. v0.1+ extends this with regex / glob matching and
/// per-component scoping; v0.0.x ships exact-match.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimMapping {
    /// JWT claim path to inspect (e.g. `groups`, `email`, `roles`).
    pub claim: String,
    /// Exact value to match against in the claim.
    pub value: String,
    /// Computeza group name the operator joins when the rule
    /// matches. Validated against the built-in groups at apply time
    /// by [`computeza-ui-server::auth::is_known_group`].
    pub local_group: String,
}

impl IdpConfig {
    /// Minimal validation -- non-empty discovery URL, client_id,
    /// redirect_uri. Full URL parsing happens at the binding's
    /// discovery step.
    ///
    /// Returns the first error found (does not collect into a Vec).
    pub fn validate(&self) -> Result<(), String> {
        if self.discovery_url.is_empty() {
            return Err("discovery_url is required".into());
        }
        if let Err(e) = url::Url::parse(&self.discovery_url) {
            return Err(format!("discovery_url is not a valid URL: {e}"));
        }
        if self.client_id.is_empty() {
            return Err("client_id is required".into());
        }
        if self.redirect_uri.is_empty() {
            return Err("redirect_uri is required".into());
        }
        if let Err(e) = url::Url::parse(&self.redirect_uri) {
            return Err(format!("redirect_uri is not a valid URL: {e}"));
        }
        for (i, m) in self.claim_mappings.iter().enumerate() {
            if m.claim.is_empty() || m.value.is_empty() || m.local_group.is_empty() {
                return Err(format!(
                    "claim_mappings[{i}]: claim, value, and local_group are all required"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idp_kind_slug_and_label_are_stable() {
        for k in IdpKind::all() {
            assert!(!k.slug().is_empty());
            assert!(!k.label().is_empty());
            // Slug should round-trip through JSON.
            let json = serde_json::to_string(k).unwrap();
            let back: IdpKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*k, back);
        }
    }

    #[test]
    fn idp_config_validates_required_fields() {
        let mut c = IdpConfig {
            kind: IdpKind::Keycloak,
            discovery_url: "https://kc.example.com/realms/x/.well-known/openid-configuration"
                .into(),
            client_id: "computeza".into(),
            client_secret_ref: Some("keycloak/client-secret".into()),
            redirect_uri: "https://console.example.com/auth/callback".into(),
            claim_mappings: vec![ClaimMapping {
                claim: "groups".into(),
                value: "computeza-admins".into(),
                local_group: "admins".into(),
            }],
        };
        assert!(c.validate().is_ok());

        c.client_id = String::new();
        assert!(c.validate().is_err());
        c.client_id = "computeza".into();

        c.discovery_url = "not a url".into();
        assert!(c.validate().is_err());
        c.discovery_url = "https://kc.example.com/.well-known/openid-configuration".into();

        c.claim_mappings.push(ClaimMapping {
            claim: String::new(),
            value: "x".into(),
            local_group: "admins".into(),
        });
        assert!(c.validate().is_err());
    }
}
