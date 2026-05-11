//! Ed25519 keypair management for the audit log.
//!
//! v0.0.x: random generation, in-memory only. v0.0.x+1 adds file-backed
//! persistence with restrictive permissions (0600 on Linux/macOS, ACL on
//! Windows) and an HSM-backed variant gated behind a feature flag.

use base64ct::{Base64, Encoding};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

/// Audit signing key.
///
/// Holds a full Ed25519 signing key. The verifying half is exposed via
/// [`AuditKey::verifying_key_b64`] for embedding in evidence packs.
pub struct AuditKey {
    signing: SigningKey,
}

impl AuditKey {
    /// Generate a fresh random keypair from the OS CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut csprng = OsRng;
        Self {
            signing: SigningKey::generate(&mut csprng),
        }
    }

    /// Construct from an existing 32-byte secret. Used by tests for
    /// deterministic signing; not exposed publicly to discourage
    /// accidental key-reuse outside test contexts.
    #[cfg(test)]
    pub(crate) fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&secret),
        }
    }

    /// Sign `bytes` with the audit key. Returns base64 (standard, no padding).
    pub fn sign(&self, bytes: &[u8]) -> String {
        let sig = self.signing.sign(bytes);
        Base64::encode_string(&sig.to_bytes())
    }

    /// Verify a signature produced by [`AuditKey::sign`]. Returns false on
    /// any failure (bad base64, wrong length, invalid signature).
    pub fn verify(verifying: &VerifyingKey, bytes: &[u8], sig_b64: &str) -> bool {
        let Ok(sig_bytes) = Base64::decode_vec(sig_b64) else {
            return false;
        };
        let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
            return false;
        };
        let sig = Signature::from_bytes(&sig_arr);
        verifying.verify(bytes, &sig).is_ok()
    }

    /// Base64-encoded verifying (public) key. Goes into the evidence
    /// pack's signed manifest so auditors can verify offline.
    #[must_use]
    pub fn verifying_key_b64(&self) -> String {
        Base64::encode_string(self.signing.verifying_key().as_bytes())
    }

    /// Decode a base64 verifying key (as produced by
    /// [`AuditKey::verifying_key_b64`]) back into the dalek type.
    pub fn verifying_key_from_b64(b64: &str) -> Option<VerifyingKey> {
        let bytes = Base64::decode_vec(b64).ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        VerifyingKey::from_bytes(&arr).ok()
    }
}

impl std::fmt::Debug for AuditKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the signing key. The verifying key is fine.
        f.debug_struct("AuditKey")
            .field("verifying_key_b64", &self.verifying_key_b64())
            .finish_non_exhaustive()
    }
}
