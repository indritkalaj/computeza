//! Post-quantum readiness reporter for the channel-partner gRPC
//! transport.
//!
//! Computeza's TLS stack is rustls 0.23 + the `aws-lc-rs` crypto
//! provider (the `tls-aws-lc` tonic feature, declared in `Cargo.toml`).
//! aws-lc-rs ships the NIST-standardised hybrid key-exchange group
//! **X25519MLKEM768** (a.k.a. `secp256r1_kyber768draft00`'s successor
//! per IETF draft-kwiatkowski-tls-ecdhe-mlkem-02). On rustls 0.23.27+
//! the hybrid group is in the default offered set, so every TLS
//! handshake Computeza initiates -- as a client or as a server --
//! offers the hybrid alongside the classical X25519.
//!
//! # What this gives us
//!
//! - **Harvest-now-decrypt-later resistance**: an attacker recording
//!   the TLS handshake today cannot recover the session key when a
//!   sufficiently large quantum computer arrives, because the ML-KEM
//!   leg of the hybrid is broken only by Shor-on-lattices, which is
//!   not believed to be efficient. Even if Shor breaks X25519 itself,
//!   the recorded handshake still requires breaking ML-KEM to
//!   produce the session key.
//!
//! - **Backward compatibility**: a classical-only peer that does not
//!   support X25519MLKEM768 still negotiates X25519. We do not break
//!   handshakes with legacy peers.
//!
//! # What this does NOT cover
//!
//! - **Authentication**: the certificate signature algorithm. Today
//!   we use classical signatures (RSA / ECDSA) in the certs. v0.1
//!   tracks issuance of dual-sig X.509 certs (classical + ML-DSA
//!   wrapper) -- when CA tooling supports it.
//!
//! - **License signing**: see [`computeza_license::License::is_pq_dual_signed`].
//!   The license envelope has its own PQ posture, independent of TLS.
//!
//! # Surfacing readiness
//!
//! The operator console renders this via the PQ-readiness card on
//! `/admin/license`. Use [`tls_readiness`] to produce the
//! displayable struct.

#![allow(missing_docs)] // returned struct fields are self-describing

/// Snapshot of the PQ posture of the binary at build time. All
/// information is static -- nothing here is read from the OS or the
/// crypto provider at runtime; we report what we know we shipped
/// against.
#[derive(Clone, Debug)]
pub struct PqReadiness {
    /// Crypto provider in use by rustls. Computeza ships with
    /// `aws-lc-rs` exclusively (see the `tls-aws-lc` tonic feature).
    pub crypto_provider: &'static str,
    /// Whether the hybrid key-exchange group X25519MLKEM768 is in
    /// the offered set on outbound TLS handshakes. True when running
    /// against rustls 0.23.27+ with the aws-lc-rs provider.
    pub tls_hybrid_kex_enabled: bool,
    /// The hybrid group identifier as offered on the wire.
    pub tls_hybrid_kex_group: &'static str,
    /// Whether the binary's license envelope verifier accepts the
    /// dual-sig (Ed25519 + ML-DSA) shape. True since v0.0.x --
    /// verification of the ML-DSA leg lands in v0.1.
    pub license_dual_sig_supported: bool,
    /// Whether the v0.0.x verifier actually validates the ML-DSA leg
    /// cryptographically. **False in v0.0.x** -- shape is checked,
    /// signature verification is a TODO pending a vetted pure-Rust
    /// ML-DSA crate in workspace.
    pub license_dual_sig_verified: bool,
}

/// Report the PQ posture of this build. Always returns the same
/// struct; no I/O, no allocations, safe to call from any context.
#[must_use]
pub fn tls_readiness() -> PqReadiness {
    PqReadiness {
        crypto_provider: "aws-lc-rs",
        tls_hybrid_kex_enabled: true,
        tls_hybrid_kex_group: "X25519MLKEM768",
        license_dual_sig_supported: true,
        license_dual_sig_verified: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_readiness_reports_hybrid_kex_enabled() {
        let r = tls_readiness();
        assert!(r.tls_hybrid_kex_enabled);
        assert_eq!(r.crypto_provider, "aws-lc-rs");
        assert_eq!(r.tls_hybrid_kex_group, "X25519MLKEM768");
    }

    #[test]
    fn tls_readiness_reports_license_dual_sig_supported_but_not_yet_verified() {
        let r = tls_readiness();
        assert!(r.license_dual_sig_supported);
        assert!(
            !r.license_dual_sig_verified,
            "v0.0.x must not claim verified PQ sigs -- shape only"
        );
    }
}
