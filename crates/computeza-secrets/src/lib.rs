//! Computeza secrets -- encrypted secret storage.
//!
//! Per spec section 3.2 this crate provides AES-256-GCM encrypted secret storage
//! with a wrapped key envelope. v0.0.x ships:
//!
//! - [`SecretsStore`]: a JSON-Lines, file-backed store where each entry's
//!   value is encrypted with AES-256-GCM under the cluster's data-encryption
//!   key (DEK). Names are stored in clear (you need them to look up the
//!   secret) but values never touch disk in plaintext.
//! - DEK derivation from a passphrase via Argon2id (`m=64MiB t=3 p=1`),
//!   matching the conservative defaults from RFC 9106. The passphrase
//!   typically comes from an env var the operator sets at install time;
//!   HSM / Vault / KMIP / PKCS#11 integrations behind feature flags
//!   (spec section 8.4) ship later.
//! - Zeroize-on-drop for in-memory plaintext so secrets vanish from
//!   memory promptly after use.
//!
//! # Boundary
//!
//! - **Not for resource specs**: those live in [`computeza-state`]. The
//!   secrets store holds only opaque secrets -- passwords, API tokens,
//!   bearer tokens -- referenced by *name* from resource specs (e.g. a
//!   `PostgresSpec` carries `superuser_password_ref: "postgres/superuser"`,
//!   and `computeza-state` would resolve that against `SecretsStore::get`
//!   before passing the spec into a reconciler).
//! - **Not for the audit log key**: the Ed25519 signing key for
//!   [`computeza-audit`] is bootstrapped separately because we want the
//!   audit log to be usable before the secrets store comes online (so
//!   the secrets-store boot itself is auditable).

#![warn(missing_docs)]

mod error;
mod kek;
mod store;

pub use error::{Result, SecretsError};
pub use kek::{derive_kek_from_passphrase, MasterKey};
pub use store::SecretsStore;
