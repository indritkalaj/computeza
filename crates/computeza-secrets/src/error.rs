//! Secrets-store errors.

use thiserror::Error;

/// Errors emitted by the secrets store.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// I/O failure on the underlying JSON-Lines file.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// AES-GCM seal / open failure — typically wrong master key (auth tag fails).
    #[error("aes-gcm: {0}")]
    Crypto(String),
    /// base64 decoding failure on the stored nonce or ciphertext.
    #[error("base64: {0}")]
    Base64(String),
    /// Argon2 KDF failure (only reachable with bad parameters).
    #[error("argon2: {0}")]
    Argon2(String),
    /// Lookup by name returned nothing.
    #[error("secret not found: {0}")]
    NotFound(String),
    /// Caller passed a master key that isn't 32 bytes.
    #[error("master key must be exactly 32 bytes, got {0}")]
    BadMasterKey(usize),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, SecretsError>;
