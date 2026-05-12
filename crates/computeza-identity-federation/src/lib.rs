//! Computeza identity federation -- OIDC client trait + upstream IdP
//! bindings.
//!
//! The operator console's local auth surface (login / setup /
//! /admin/operators) ships in [`computeza-ui-server`]. This crate is
//! the missing layer that lets the platform federate identity
//! *upward* through an external IdP -- Entra ID, AWS IAM Identity
//! Center, GCP Identity Platform, Keycloak, or an on-prem LDAP /
//! Kerberos bridge -- so an operator's enterprise sign-on credential
//! grants them console access without a separate per-Computeza
//! password.
//!
//! # What ships in v0.0.x
//!
//! - [`IdpKind`] -- closed enum of the IdP flavors we plan to wire
//!   first (`EntraId`, `AwsIam`, `GcpIam`, `Keycloak`, `GenericOidc`).
//! - [`IdpConfig`] -- serializable per-tenant configuration carrying
//!   discovery URL, client_id, redirect URI, and a `secret_ref`
//!   pointing into the encrypted [`computeza-secrets`] store for the
//!   client secret.
//! - [`IdentityProvider`] -- async trait every IdP binding implements:
//!   build the authorization URL, exchange the auth code, verify the
//!   ID token, return canonical claims.
//! - [`Claims`] -- normalized user claims (subject, email, groups,
//!   issuer, expiry) every binding produces so downstream consumers
//!   don't have to switch on the IdP kind.
//!
//! # What is explicitly scaffolded (no real impl yet)
//!
//! Every [`IdentityProvider`] implementation in this crate stubs out
//! to [`IdpError::NotImplemented`]. v0.1+ fills in the OIDC discovery,
//! JWT verification, and token-exchange code paths per IdP. The shape
//! lands here so the operator console can render an "Identity and
//! access" form against a real type (rather than a placeholder
//! disclosure) and the kanidm reconciler can declare a federation
//! binding without waiting on the full implementation.
//!
//! # Boundary
//!
//! - **Not the password store.** Local operator passwords live in
//!   [`computeza-ui-server::auth::OperatorFile`]. Federation is the
//!   alternative auth path; the two coexist on the login page once
//!   v0.1 wires this.
//! - **Not the authz layer.** Federation produces *claims*; the
//!   RBAC layer ([`computeza-ui-server::auth::Permission`]) decides
//!   what those claims can do. A federated session maps onto one or
//!   more local groups by claim-to-group rules persisted alongside
//!   the IdP config.

#![warn(missing_docs)]

mod claims;
mod config;
mod error;
mod provider;
mod providers;

pub use claims::{Claims, GroupClaim};
pub use config::{ClaimMapping, IdpConfig, IdpKind};
pub use error::IdpError;
pub use provider::{AuthorizationRequest, IdentityProvider, TokenResponse};
pub use providers::{
    AwsIamProvider, EntraIdProvider, GcpIamProvider, GenericOidcProvider, KeycloakProvider,
};

/// Pick the right concrete [`IdentityProvider`] for an [`IdpConfig`].
/// Constructs a provider whose `provider_kind` matches the config; v0.1
/// fills in the per-IdP discovery + verification logic.
#[must_use]
pub fn provider_for_config(config: IdpConfig) -> Box<dyn IdentityProvider> {
    match config.kind {
        IdpKind::EntraId => Box::new(EntraIdProvider::new(config)),
        IdpKind::AwsIam => Box::new(AwsIamProvider::new(config)),
        IdpKind::GcpIam => Box::new(GcpIamProvider::new(config)),
        IdpKind::Keycloak => Box::new(KeycloakProvider::new(config)),
        IdpKind::GenericOidc => Box::new(GenericOidcProvider::new(config)),
    }
}
