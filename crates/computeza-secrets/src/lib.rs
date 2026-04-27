//! Computeza secrets — encrypted secret storage.
//!
//! Per spec §3.2, this crate provides AES-256-GCM encrypted secret storage
//! with age key wrapping. Customer-managed key (CMK) integration for
//! HashiCorp Vault, KMIP-compliant HSMs, and PKCS#11 (spec §8.4) plugs in
//! through a `KeyProvider` trait that lives here.
//!
//! Scaffold stub. Implementation is pending.

#![warn(missing_docs)]
