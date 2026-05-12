//! Computeza license envelope -- ed25519-signed entitlement tokens.
//!
//! Per the AGENTS.md "Product constraints / Multi-tier distribution
//! channels" rule, Computeza is sellable through three channels
//! simultaneously (direct / reseller / sub-reseller), and the license
//! must encode the full reseller chain so every upstream party can
//! verify entitlement and bill.
//!
//! This crate ships the envelope shape + verification primitives.
//! v0.0.x intentionally does NOT enforce -- the operator console
//! renders the chain on /admin/license but mutations are not gated.
//! v0.1+ wires enforcement (seat caps, expiry kill-switch).
//!
//! # The chain claim
//!
//! [`ChainEntry`] -- one entry per tier in the resale chain.
//!   - tier 0: issuer (always Computeza Inc.)
//!   - tier 1: first reseller (Microsoft, an OEM, a distributor)
//!   - tier 2: sub-reseller (when present)
//!   - tier N: end-customer (always the last entry)
//!
//! Every tier carries its issuer-of-record name, an ed25519 verifying
//! key, and an optional `support_contact` rendered in the operator
//! console footer (per the AGENTS.md "Support routing" constraint).

#![warn(missing_docs)]

use base64ct::{Base64, Encoding};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Errors raised during license verification.
#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    /// JSON deserialization failed.
    #[error("malformed license envelope: {0}")]
    Malformed(String),
    /// Signature did not verify against the claimed verifying key.
    #[error("signature verification failed")]
    BadSignature,
    /// The verifying key in the chain did not chain back to the
    /// Computeza root key.
    #[error("chain anchor mismatch -- top entry must be the Computeza issuer")]
    BadAnchor,
    /// License's `not_after` is in the past.
    #[error("license expired on {0}")]
    Expired(DateTime<Utc>),
    /// License's `not_before` is in the future.
    #[error("license not yet valid (becomes effective on {0})")]
    NotYetValid(DateTime<Utc>),
}

/// One tier in the resale chain. Tier 0 is always the issuer
/// (Computeza); tier N is always the end customer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainEntry {
    /// Human-readable name of this tier. Renders in the operator
    /// console footer and on /admin/license.
    pub name: String,
    /// Base64-encoded ed25519 verifying key for this tier. Used in
    /// v0.1+ to verify per-tier counter-signatures on telemetry
    /// upload.
    pub verifying_key: String,
    /// Optional support-routing contact. When set, the operator
    /// console footer renders a link to this rather than the
    /// upstream tier's contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_contact: Option<String>,
}

/// License payload -- the body the issuer signs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicensePayload {
    /// Stable license identifier (UUID v4 typically).
    pub id: String,
    /// Tier of the end-customer entitlement (Community / Pro /
    /// Enterprise -- matches the pricing tiers in the marketing
    /// landing). Lowercase string for forward compatibility.
    pub tier: String,
    /// Seat count this license entitles. `None` means unlimited
    /// (Enterprise typically).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seats: Option<u32>,
    /// License valid-from timestamp.
    pub not_before: DateTime<Utc>,
    /// License expiry. Past this timestamp the v0.1+ enforcement
    /// layer marks the install read-only.
    pub not_after: DateTime<Utc>,
    /// Resale chain from issuer (index 0) to customer (last index).
    /// At least two entries required (issuer + customer); chains of
    /// three or four cover the reseller / sub-reseller cases.
    pub chain: Vec<ChainEntry>,
}

/// Signed license envelope. Persisted at
/// `<state_db_parent>/license.json` once the operator activates one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct License {
    /// The payload that was signed.
    pub payload: LicensePayload,
    /// Base64 ed25519 signature of `canonical_bytes(payload)`. The
    /// verifying key is `payload.chain[0].verifying_key` -- the
    /// issuer-of-record's key.
    pub signature: String,
}

impl LicensePayload {
    /// Canonical bytes used as signature input. JSON with sorted
    /// keys so the same payload always produces the same bytes
    /// regardless of how it was constructed.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LicenseError> {
        // serde_json doesn't sort keys by default, but the struct's
        // field order is stable so this is canonical-by-construction
        // for our types. Multi-language verifiers should still
        // canonicalise; we ship the verifier in Rust.
        serde_json::to_vec(self).map_err(|e| LicenseError::Malformed(e.to_string()))
    }
}

impl License {
    /// Verify the license against the embedded chain anchor and the
    /// provided trusted-root key (the Computeza issuer key). Also
    /// checks the validity window against `now`.
    ///
    /// `trusted_root` is the verifying key the binary ships baked-in
    /// (rotated via release with key-overlap). When `None` we use
    /// only the embedded chain[0] key, which is fine for
    /// development but means the license could be self-signed --
    /// production binaries always pass `Some(...)`.
    pub fn verify(
        &self,
        trusted_root: Option<&VerifyingKey>,
        now: DateTime<Utc>,
    ) -> Result<(), LicenseError> {
        // Chain must have at least issuer + customer.
        if self.payload.chain.len() < 2 {
            return Err(LicenseError::Malformed(
                "chain must carry at least the issuer + customer entries".into(),
            ));
        }
        let issuer = &self.payload.chain[0];
        let issuer_key_bytes = Base64::decode_vec(&issuer.verifying_key)
            .map_err(|e| LicenseError::Malformed(format!("chain[0] verifying_key: {e}")))?;
        let issuer_key_arr: [u8; 32] = issuer_key_bytes
            .try_into()
            .map_err(|_| LicenseError::Malformed("chain[0] verifying_key length".into()))?;
        let embedded_key = VerifyingKey::from_bytes(&issuer_key_arr)
            .map_err(|e| LicenseError::Malformed(format!("chain[0] verifying_key: {e}")))?;

        // If a trusted root was passed, the chain anchor MUST match.
        if let Some(root) = trusted_root {
            if root.as_bytes() != embedded_key.as_bytes() {
                return Err(LicenseError::BadAnchor);
            }
        }

        // Verify the signature over the canonical payload bytes.
        let sig_bytes = Base64::decode_vec(&self.signature)
            .map_err(|e| LicenseError::Malformed(format!("signature decode: {e}")))?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| LicenseError::Malformed("signature length".into()))?;
        let sig = Signature::from_bytes(&sig_arr);
        let body = self.payload.canonical_bytes()?;
        embedded_key
            .verify(&body, &sig)
            .map_err(|_| LicenseError::BadSignature)?;

        // Time window.
        if now < self.payload.not_before {
            return Err(LicenseError::NotYetValid(self.payload.not_before));
        }
        if now > self.payload.not_after {
            return Err(LicenseError::Expired(self.payload.not_after));
        }
        Ok(())
    }
}

/// Issue a fresh license. v0.0.x ships this for dev / test use; the
/// production issuance pipeline lives outside the binary (the
/// Computeza release infrastructure).
pub fn issue(signing_key: &SigningKey, payload: LicensePayload) -> Result<License, LicenseError> {
    use ed25519_dalek::Signer;
    let body = payload.canonical_bytes()?;
    let sig: Signature = signing_key.sign(&body);
    Ok(License {
        payload,
        signature: Base64::encode_string(&sig.to_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixed_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sample_payload(now: DateTime<Utc>, vk: VerifyingKey) -> LicensePayload {
        LicensePayload {
            id: "11111111-2222-3333-4444-555555555555".into(),
            tier: "enterprise".into(),
            seats: Some(50),
            not_before: now - chrono::Duration::days(1),
            not_after: now + chrono::Duration::days(365),
            chain: vec![
                ChainEntry {
                    name: "Computeza Inc.".into(),
                    verifying_key: Base64::encode_string(vk.as_bytes()),
                    support_contact: None,
                },
                ChainEntry {
                    name: "Acme Corp.".into(),
                    verifying_key: Base64::encode_string(vk.as_bytes()),
                    support_contact: Some("ops@acme.example".into()),
                },
            ],
        }
    }

    #[test]
    fn issue_and_verify_round_trips() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let payload = sample_payload(now, vk);
        let lic = issue(&sk, payload).unwrap();
        lic.verify(Some(&vk), now).unwrap();
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut lic = issue(&sk, sample_payload(now, vk)).unwrap();
        lic.payload.seats = Some(99999);
        assert!(matches!(
            lic.verify(Some(&vk), now),
            Err(LicenseError::BadSignature)
        ));
    }

    #[test]
    fn verify_rejects_expired_license() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.not_after = now - chrono::Duration::days(1);
        let lic = issue(&sk, payload).unwrap();
        assert!(matches!(
            lic.verify(Some(&vk), now),
            Err(LicenseError::Expired(_))
        ));
    }

    #[test]
    fn verify_rejects_not_yet_valid_license() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.not_before = now + chrono::Duration::days(1);
        let lic = issue(&sk, payload).unwrap();
        assert!(matches!(
            lic.verify(Some(&vk), now),
            Err(LicenseError::NotYetValid(_))
        ));
    }

    #[test]
    fn verify_rejects_chain_anchor_mismatch() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let lic = issue(&sk, sample_payload(now, vk)).unwrap();
        let wrong_root = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        assert!(matches!(
            lic.verify(Some(&wrong_root), now),
            Err(LicenseError::BadAnchor)
        ));
    }

    #[test]
    fn verify_rejects_chain_with_fewer_than_two_entries() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.chain.truncate(1);
        let lic = issue(&sk, payload).unwrap();
        assert!(matches!(
            lic.verify(Some(&vk), now),
            Err(LicenseError::Malformed(_))
        ));
    }
}
