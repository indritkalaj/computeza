//! mTLS scaffolding for the channel-partner server.
//!
//! Resellers and OEMs authenticate to the channel-partner endpoint
//! with an mTLS client cert. The CN identifies the tier; v0.1+ wires
//! the cert -> license-chain-entry mapping that authorizes per-tenant
//! operations.
//!
//! v0.0.x ships the cert-loading + tls-config construction; serving
//! happens when the operator-console binary boots the gRPC service
//! on a dedicated port.

use std::path::Path;

use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// Errors raised while loading TLS material from disk.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// PEM file read failed (missing file, permission denied, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Build a `ServerTlsConfig` from three PEM files:
/// - `server_cert` -- server certificate the gRPC endpoint presents.
/// - `server_key` -- server private key matching the cert.
/// - `client_ca` -- bundle of trusted client-CA certs. Clients must
///   present a cert chained to one of these.
///
/// The config is ready to feed into `Server::builder().tls_config(...)`.
pub async fn load_server_tls(
    server_cert: &Path,
    server_key: &Path,
    client_ca: &Path,
) -> Result<ServerTlsConfig, TlsError> {
    let cert_pem = tokio::fs::read(server_cert).await?;
    let key_pem = tokio::fs::read(server_key).await?;
    let ca_pem = tokio::fs::read(client_ca).await?;

    let identity = Identity::from_pem(cert_pem, key_pem);
    let ca = Certificate::from_pem(ca_pem);

    Ok(ServerTlsConfig::new().identity(identity).client_ca_root(ca))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_server_tls_surfaces_missing_files() {
        let dir = std::env::temp_dir().join(format!(
            "computeza-test-tls-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cert = dir.join("server.crt");
        let key = dir.join("server.key");
        let ca = dir.join("ca.crt");
        let err = load_server_tls(&cert, &key, &ca).await.unwrap_err();
        assert!(matches!(err, TlsError::Io(_)));
    }
}
