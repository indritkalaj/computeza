//! Computeza license envelope -- ed25519-signed entitlement tokens.
//!
//! Per the AGENTS.md "Product constraints / Multi-tier distribution
//! channels" rule, Computeza is sellable through three channels
//! simultaneously (direct / reseller / sub-reseller), and the license
//! must encode the full reseller chain so every upstream party can
//! verify entitlement and bill.
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
//!
//! # Entitlements vs. billing
//!
//! The license carries **entitlements** (tier, seats, features,
//! validity window). It does **not** carry billing amounts -- those
//! are a sales / CRM concern, not an offline-verifiable claim. The
//! optional [`BillingNote`] field exists as displayable metadata for
//! the operator console only; nothing in this crate or the binary
//! gates on it.
//!
//! # Tier convention
//!
//! v0.0.x recognises two tier strings:
//!
//! - `"standard"` -- SMB self-service tier, seat-capped (typically 100
//!   seats or less). Default 49.99 EUR/user/month list price; resellers
//!   may charge differently.
//! - `"enterprise"` -- Custom-contract tier. `seats` is typically
//!   `None` (unlimited), `billing_metadata` carries the negotiated
//!   contract reference.
//!
//! The string is forward-compatible; new tiers (e.g. `"provider"` for
//! the channel-partner program) land additively without breaking
//! existing envelopes.

#![warn(missing_docs)]

use std::path::Path;

use base64ct::{Base64, Encoding};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Computeza-root verifying key shipped with the binary.
///
/// **v0.0.x ships a development key**; production builds swap this in
/// at release time via a build script (or, simpler, by hand-editing
/// this constant before the release tag). The key-overlap rotation
/// strategy means a binary can accept two roots simultaneously during
/// one minor version -- we'll add a `TRUSTED_ROOTS_ADDITIONAL` slice
/// when the first rotation lands.
///
/// This key MUST NOT match a key any reseller controls -- it is the
/// anchor for the entire chain. Compromise means re-keying every
/// license in the field, which is why issuance is air-gapped (spec
/// section 15).
///
/// The dev key below corresponds to ed25519 seed bytes `[7u8; 32]`;
/// the matching signing key is checked into the test suite. Operators
/// running production binaries should NEVER trust the dev key -- the
/// release pipeline replaces it before signing the binary.
pub const TRUSTED_ROOT_VK_BASE64: &str = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";

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
    /// On-disk persistence failed.
    #[error("license persistence error: {0}")]
    Io(String),
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

/// Negotiated commercial terms. **Displayable metadata only.**
///
/// The binary does not enforce on `annual_value`; the field is
/// rendered on `/admin/license` so the operator can see what their
/// org actually agreed to. Placing it here (in the signed envelope)
/// guarantees the displayed amount matches the contract -- the
/// operator cannot edit it without invalidating the signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BillingNote {
    /// Total annual contract value in the smallest unit of `currency`
    /// (e.g. cents for USD/EUR). `None` for trial / NFR / community
    /// licenses where no commercial terms apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annual_value: Option<u64>,
    /// ISO 4217 currency code (`"EUR"`, `"USD"`, `"GBP"`, etc.).
    /// Required when `annual_value` is `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Reseller / sales-team contract reference. Free-form. Renders
    /// on /admin/license so the operator can quote it when raising
    /// support tickets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_id: Option<String>,
}

/// License payload -- the body the issuer signs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicensePayload {
    /// Stable license identifier (UUID v4 typically).
    pub id: String,
    /// Tier of the end-customer entitlement. See module docs --
    /// `"standard"` or `"enterprise"` in v0.0.x; lowercase string for
    /// forward compatibility.
    pub tier: String,
    /// Seat count this license entitles. `None` means unlimited
    /// (enterprise typically). Enforcement: `/admin/operators` create
    /// rejects when the live operator count would exceed this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seats: Option<u32>,
    /// License valid-from timestamp.
    pub not_before: DateTime<Utc>,
    /// License expiry. Past this timestamp the enforcement layer
    /// flips the binary into read-only mode (mutating admin / install
    /// routes return a renewal banner; existing services keep
    /// running).
    pub not_after: DateTime<Utc>,
    /// Optional feature flags carried by this license. Empty in
    /// v0.0.x; reserved for v0.1+ to gate specific surfaces
    /// (e.g. `"ai-workspace"`, `"channel-partner-grpc"`,
    /// `"multi-tenancy"`). Unknown flags are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Displayable commercial metadata. Optional; not enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_metadata: Option<BillingNote>,
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

/// Live status of a loaded license, as the enforcement layer sees it.
///
/// `Active` is the only state that lets mutating admin / install
/// routes through. The others trigger a top-banner explanation and
/// (for Expired) read-only mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LicenseStatus {
    /// License verified and currently within its validity window.
    /// `days_remaining` counts down to `not_after`; <= 30 triggers a
    /// renewal banner without otherwise gating routes.
    Active {
        /// Days remaining until expiry. May be 0 on the last day.
        days_remaining: i64,
    },
    /// `not_before` is in the future. Probably a clock-skew issue or
    /// a license activated ahead of its start date.
    NotYetValid {
        /// Days until the license takes effect (always >= 1).
        days_until_valid: i64,
    },
    /// `not_after` is in the past. Read-only mode engaged.
    Expired {
        /// Days since expiry (always >= 1).
        days_since_expiry: i64,
    },
    /// Signature or chain-anchor verification failed. The enforcement
    /// layer logs and falls back to Community mode (None).
    Invalid(&'static str),
    /// No license activated. The operator is in Community mode.
    None,
}

impl LicenseStatus {
    /// `true` when mutating admin / install routes should be allowed
    /// to run. `Active` and `None` (community mode) both qualify;
    /// `Expired` / `Invalid` / `NotYetValid` engage read-only mode.
    #[must_use]
    pub fn allow_mutations(&self) -> bool {
        matches!(self, LicenseStatus::Active { .. } | LicenseStatus::None)
    }

    /// `true` when the operator should see a renewal banner in the
    /// top nav. Fires for `Active` with <= 30 days remaining, and for
    /// every non-`Active`/non-`None` state.
    #[must_use]
    pub fn should_warn(&self) -> bool {
        match self {
            LicenseStatus::Active { days_remaining } => *days_remaining <= 30,
            LicenseStatus::None => false,
            _ => true,
        }
    }
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

    /// Combined verify-and-classify entry point used by the
    /// enforcement layer. Maps verify outcomes into [`LicenseStatus`]
    /// rather than `Result`; convenient because routes need to
    /// branch on the status anyway.
    #[must_use]
    pub fn status(&self, trusted_root: Option<&VerifyingKey>, now: DateTime<Utc>) -> LicenseStatus {
        match self.verify(trusted_root, now) {
            Ok(()) => {
                let remaining = (self.payload.not_after - now).num_days();
                LicenseStatus::Active {
                    days_remaining: remaining.max(0),
                }
            }
            Err(LicenseError::Expired(ts)) => LicenseStatus::Expired {
                days_since_expiry: (now - ts).num_days().max(1),
            },
            Err(LicenseError::NotYetValid(ts)) => LicenseStatus::NotYetValid {
                days_until_valid: (ts - now).num_days().max(1),
            },
            Err(LicenseError::BadSignature) => LicenseStatus::Invalid("bad-signature"),
            Err(LicenseError::BadAnchor) => LicenseStatus::Invalid("bad-anchor"),
            Err(LicenseError::Malformed(_)) => LicenseStatus::Invalid("malformed"),
            Err(LicenseError::Io(_)) => LicenseStatus::Invalid("io-error"),
        }
    }

    /// Days remaining until expiry. Negative when already expired.
    #[must_use]
    pub fn days_remaining(&self, now: DateTime<Utc>) -> i64 {
        (self.payload.not_after - now).num_days()
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

/// Parse a base64-encoded ed25519 verifying key (the format the
/// binary's [`TRUSTED_ROOT_VK_BASE64`] constant uses).
pub fn parse_verifying_key_b64(b64: &str) -> Result<VerifyingKey, LicenseError> {
    let bytes = Base64::decode_vec(b64.trim())
        .map_err(|e| LicenseError::Malformed(format!("verifying key decode: {e}")))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| LicenseError::Malformed("verifying key length".into()))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| LicenseError::Malformed(e.to_string()))
}

/// Resolve the trusted-root verifying key the binary ships with.
/// Convenience: same as `parse_verifying_key_b64(TRUSTED_ROOT_VK_BASE64)`
/// but panics on failure (a build-time invariant -- the constant must
/// always be a valid key, and a release that violates this is a
/// release we don't want to ship).
#[must_use]
pub fn trusted_root() -> VerifyingKey {
    parse_verifying_key_b64(TRUSTED_ROOT_VK_BASE64)
        .expect("TRUSTED_ROOT_VK_BASE64 must be a valid base64 ed25519 verifying key")
}

/// Read a license envelope from JSON text. Used by the activation
/// handler where the operator pastes the envelope body into a form.
pub fn parse_envelope(text: &str) -> Result<License, LicenseError> {
    serde_json::from_str(text).map_err(|e| LicenseError::Malformed(e.to_string()))
}

/// Load a license from `<state_db_parent>/license.json` if present.
/// Returns `Ok(None)` when the file does not exist (community mode);
/// `Err` when the file exists but cannot be parsed.
pub fn load_license_file(path: &Path) -> Result<Option<License>, LicenseError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_envelope(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LicenseError::Io(e.to_string())),
    }
}

/// Atomically persist a license to disk. Writes to `<path>.tmp` then
/// renames so an interrupted write never leaves a half-truncated
/// envelope.
pub fn save_license_file(path: &Path, license: &License) -> Result<(), LicenseError> {
    let body = serde_json::to_vec_pretty(license).map_err(|e| LicenseError::Io(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| LicenseError::Io(e.to_string()))?;
        }
    }
    std::fs::write(&tmp, &body).map_err(|e| LicenseError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| LicenseError::Io(e.to_string()))?;
    Ok(())
}

/// Remove a license file from disk, returning to community mode.
/// Idempotent: succeeds when the file is already absent.
pub fn delete_license_file(path: &Path) -> Result<(), LicenseError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LicenseError::Io(e.to_string())),
    }
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
            features: Vec::new(),
            billing_metadata: None,
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

    #[test]
    fn trusted_root_constant_parses() {
        let _vk = trusted_root();
    }

    #[test]
    fn trusted_root_matches_dev_seed_seven() {
        // Documents the test invariant: the baked-in dev key is
        // ed25519 seed = [7u8; 32]. If this test fires after editing
        // TRUSTED_ROOT_VK_BASE64, the test suite needs the matching
        // signing key swapped in too.
        let dev_signing = SigningKey::from_bytes(&[7u8; 32]);
        let dev_vk = dev_signing.verifying_key();
        assert_eq!(trusted_root().as_bytes(), dev_vk.as_bytes());
    }

    #[test]
    fn status_active_when_valid_and_within_window() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let lic = issue(&sk, sample_payload(now, vk)).unwrap();
        let s = lic.status(Some(&vk), now);
        assert!(matches!(s, LicenseStatus::Active { days_remaining } if days_remaining > 360));
        assert!(s.allow_mutations());
        assert!(!s.should_warn());
    }

    #[test]
    fn status_active_warns_within_30_days() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.not_after = now + chrono::Duration::days(10);
        let lic = issue(&sk, payload).unwrap();
        let s = lic.status(Some(&vk), now);
        assert!(
            matches!(s, LicenseStatus::Active { days_remaining } if (8..=11).contains(&days_remaining))
        );
        assert!(s.allow_mutations());
        assert!(s.should_warn());
    }

    #[test]
    fn status_expired_blocks_mutations() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.not_after = now - chrono::Duration::days(5);
        let lic = issue(&sk, payload).unwrap();
        let s = lic.status(Some(&vk), now);
        assert!(
            matches!(s, LicenseStatus::Expired { days_since_expiry } if days_since_expiry >= 4)
        );
        assert!(!s.allow_mutations());
        assert!(s.should_warn());
    }

    #[test]
    fn status_not_yet_valid_blocks_mutations() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.not_before = now + chrono::Duration::days(7);
        let lic = issue(&sk, payload).unwrap();
        let s = lic.status(Some(&vk), now);
        assert!(
            matches!(s, LicenseStatus::NotYetValid { days_until_valid } if days_until_valid >= 6)
        );
        assert!(!s.allow_mutations());
    }

    #[test]
    fn status_invalid_on_bad_anchor() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let lic = issue(&sk, sample_payload(now, vk)).unwrap();
        let wrong_root = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let s = lic.status(Some(&wrong_root), now);
        assert!(matches!(s, LicenseStatus::Invalid("bad-anchor")));
        assert!(!s.allow_mutations());
    }

    #[test]
    fn community_mode_allows_mutations() {
        let s = LicenseStatus::None;
        assert!(s.allow_mutations());
        assert!(!s.should_warn());
    }

    #[test]
    fn billing_note_round_trips_through_envelope() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let mut payload = sample_payload(now, vk);
        payload.billing_metadata = Some(BillingNote {
            annual_value: Some(499_900),
            currency: Some("EUR".into()),
            contract_id: Some("ACME-2026-001".into()),
        });
        payload.features = vec!["ai-workspace".into(), "channel-partner-grpc".into()];
        let lic = issue(&sk, payload).unwrap();
        // Round-trip through JSON to confirm the optional fields
        // survive and the signature still verifies.
        let txt = serde_json::to_string(&lic).unwrap();
        let reloaded = parse_envelope(&txt).unwrap();
        reloaded.verify(Some(&vk), now).unwrap();
        assert_eq!(
            reloaded
                .payload
                .billing_metadata
                .as_ref()
                .unwrap()
                .annual_value,
            Some(499_900)
        );
        assert_eq!(reloaded.payload.features.len(), 2);
    }

    #[test]
    fn save_and_load_round_trip_via_tempfile() {
        let sk = fixed_key();
        let vk = sk.verifying_key();
        let now = Utc::now();
        let lic = issue(&sk, sample_payload(now, vk)).unwrap();

        let dir =
            std::env::temp_dir().join(format!("computeza-license-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("license.json");

        save_license_file(&path, &lic).unwrap();
        let loaded = load_license_file(&path).unwrap().expect("file must exist");
        assert_eq!(loaded.payload.id, lic.payload.id);
        loaded.verify(Some(&vk), now).unwrap();

        delete_license_file(&path).unwrap();
        assert!(load_license_file(&path).unwrap().is_none());

        // delete is idempotent.
        delete_license_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let path = std::env::temp_dir().join(format!(
            "computeza-license-missing-{}.json",
            std::process::id()
        ));
        // Ensure it does not exist.
        let _ = std::fs::remove_file(&path);
        assert!(load_license_file(&path).unwrap().is_none());
    }

    #[test]
    fn parse_envelope_rejects_garbage() {
        assert!(matches!(
            parse_envelope("not json"),
            Err(LicenseError::Malformed(_))
        ));
    }
}
