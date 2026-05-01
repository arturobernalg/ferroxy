//! TLS termination via rustls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

/// Errors surfaced from cert/key loading and from the TLS handshake.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    /// Cert or key file could not be opened or read.
    #[error("read tls file `{path}`")]
    ReadFile {
        /// Path attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// PEM file did not contain anything we can use.
    #[error("file `{path}` is not a valid PEM cert or key")]
    InvalidPem {
        /// Path that parsed empty.
        path: PathBuf,
    },

    /// Certificate-chain file contained zero certificates.
    #[error("cert file `{path}` contained no CERTIFICATE blocks")]
    NoCert {
        /// Path that resolved to an empty cert chain.
        path: PathBuf,
    },

    /// Private-key file contained no usable key.
    #[error("key file `{path}` contained no PKCS8 / RSA / EC PRIVATE KEY")]
    NoKey {
        /// Path that resolved to no key.
        path: PathBuf,
    },

    /// `rustls::ServerConfig` builder rejected the cert + key combo.
    #[error("rustls rejected cert/key for sni `{sni}`")]
    Rustls {
        /// SNI for which the cert was loaded (informational).
        sni: String,
        /// Underlying rustls error.
        #[source]
        source: rustls::Error,
    },

    /// TLS handshake against an accepted TCP connection failed.
    #[error("tls handshake failed")]
    Handshake(#[source] std::io::Error),

    /// `[tls]` block was expected (HTTPS listener configured) but
    /// the config has no certs.
    #[error("[tls] section is empty or missing")]
    NoCerts,
}

/// `tokio_rustls::TlsAcceptor` is the cheap-to-clone handle the
/// binary holds for accepting TLS handshakes; the inner
/// `rustls::ServerConfig` is `Arc`-backed.
pub use tokio_rustls::TlsAcceptor;

/// Load + parse the cert chain + key for the *first* `[[tls.certs]]`
/// entry and build a `rustls::ServerConfig`. Subsequent entries are
/// logged and ignored (SNI selection lands in P6.x).
///
/// Returns a `ServerConfig` that requires no client cert and accepts
/// either TLS 1.2 or 1.3 per the `tls.min_version` config field.
pub fn load_server_config(cfg: &conduit_config::TlsConfig) -> Result<ServerConfig, TlsError> {
    let cert_spec = cfg.certs.first().ok_or(TlsError::NoCerts)?;
    if cfg.certs.len() > 1 {
        tracing::warn!(
            extra = cfg.certs.len() - 1,
            "multiple [[tls.certs]] entries; P6 uses only the first \
             (SNI selection is deferred to P6.x)",
        );
    }
    let certs = load_certs(&cert_spec.cert)?;
    let key = load_key(&cert_spec.key)?;

    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|source| TlsError::Rustls {
            sni: cert_spec.sni.clone(),
            source,
        })?;
    Ok(server_cfg)
}

/// Build a `TlsAcceptor` ready to wrap incoming TCP streams.
pub fn build_acceptor(server_cfg: ServerConfig) -> TlsAcceptor {
    TlsAcceptor::from(Arc::new(server_cfg))
}

/// Wrap an accepted TCP stream with the supplied acceptor and run
/// the TLS handshake. Returns the decrypted stream on success.
pub async fn accept_tls(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
) -> Result<TlsStream<TcpStream>, TlsError> {
    acceptor.accept(stream).await.map_err(TlsError::Handshake)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut certs = Vec::new();
    for item in CertificateDer::pem_slice_iter(&bytes) {
        match item {
            Ok(c) => certs.push(c),
            Err(e) => {
                return Err(TlsError::ReadFile {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                });
            }
        }
    }
    if certs.is_empty() {
        return Err(TlsError::NoCert {
            path: path.to_path_buf(),
        });
    }
    Ok(certs)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    PrivateKeyDer::from_pem_slice(&bytes).map_err(|e| {
        // pki-types' pem::Error includes a "no key found" variant, but
        // surface a path-aware error rather than the bare enum.
        if matches!(e, rustls_pki_types::pem::Error::NoItemsFound) {
            TlsError::NoKey {
                path: path.to_path_buf(),
            }
        } else {
            TlsError::ReadFile {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_surfaces_path() {
        let path = PathBuf::from("/nonexistent/conduit-test/cert.pem");
        let err = load_certs(&path).unwrap_err();
        match err {
            TlsError::ReadFile { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected ReadFile, got {other:?}"),
        }
    }
}
