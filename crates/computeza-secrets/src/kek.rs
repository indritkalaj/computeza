//! Master-key (KEK) handling.
//!
//! The simplest production-shaped flow:
//!
//! 1. Operator sets `COMPUTEZA_SECRETS_PASSPHRASE` in the install
//!    environment.
//! 2. At first install, the platform generates a random 16-byte salt and
//!    stores it on disk next to the secrets file.
//! 3. The KEK (32 bytes) is derived via Argon2id from passphrase+salt.
//! 4. The KEK is held in a [`MasterKey`] for the lifetime of the process
//!    and zeroized on drop.
//!
//! HSM / Vault / KMIP / PKCS#11 variants (spec §8.4) plug in by
//! implementing a small trait `KeyProvider` that returns a `MasterKey` —
//! that trait + integrations land in a follow-up.

use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Result, SecretsError};

/// 32-byte symmetric data-encryption key (DEK). Wrapped in a Zeroize-on-drop
/// guard so it doesn't linger in memory after the secrets store closes.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey {
    bytes: [u8; 32],
}

impl MasterKey {
    /// Construct from a raw 32-byte slice. Returns `BadMasterKey` if the
    /// slice is the wrong length.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = b
            .try_into()
            .map_err(|_| SecretsError::BadMasterKey(b.len()))?;
        Ok(Self { bytes: arr })
    }

    /// View as a 32-byte slice (for hand-off to `Aes256Gcm::new`).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key bytes.
        f.debug_struct("MasterKey").finish_non_exhaustive()
    }
}

/// Derive a [`MasterKey`] from a passphrase + salt via Argon2id.
///
/// `salt` must be at least 8 bytes; the platform persists a 16-byte
/// random salt alongside the encrypted secrets file.
///
/// Defaults: `m=65536KiB (64 MiB), t=3, p=1` — the OWASP 2025 / RFC 9106
/// "small parameter set" baseline. Adjust upward only when latency budget
/// allows; never downward.
pub fn derive_kek_from_passphrase(passphrase: &[u8], salt: &[u8]) -> Result<MasterKey> {
    let params =
        Params::new(65_536, 3, 1, Some(32)).map_err(|e| SecretsError::Argon2(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| SecretsError::Argon2(e.to_string()))?;
    let key = MasterKey::from_bytes(&out)?;
    out.zeroize();
    Ok(key)
}
