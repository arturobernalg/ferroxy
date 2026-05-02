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

/// Build a `rustls::ServerConfig` from every `[[tls.certs]]` entry in
/// the config. Each entry contributes one cert+key bound to its SNI
/// pattern; at handshake time the resolver matches the client's SNI
/// against the table.
///
/// SNI matching is two-pass:
///   1. case-insensitive exact match against the SNI patterns
///   2. wildcard match (`*.foo.com` matches one DNS label below `foo.com`)
///
/// When no SNI is supplied (or when nothing matches), the first cert
/// declared in the config is used.
///
/// Returns a `ServerConfig` that requires no client cert and accepts
/// either TLS 1.2 or 1.3 per the `tls.min_version` config field.
pub fn load_server_config(cfg: &conduit_config::TlsConfig) -> Result<ServerConfig, TlsError> {
    let (server_cfg, _) = load_server_config_with_resolver(cfg)?;
    Ok(server_cfg)
}

/// Like [`load_server_config`] but also returns the resolver handle.
/// The caller can hold onto the `Arc<MultiCertResolver>` and call
/// [`MultiCertResolver::reload`] on SIGHUP to swap certs without
/// restarting the listener.
pub fn load_server_config_with_resolver(
    cfg: &conduit_config::TlsConfig,
) -> Result<(ServerConfig, Arc<MultiCertResolver>), TlsError> {
    if cfg.certs.is_empty() {
        return Err(TlsError::NoCerts);
    }
    let resolver = Arc::new(MultiCertResolver::from_config(&cfg.certs)?);

    let mut server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&resolver) as _);
    // Advertise HTTP/2 first, falling back to HTTP/1.1. RFC 7540 §3.3
    // requires h2 over TLS to be ALPN-negotiated, and most clients
    // pick whichever the server lists first that they support.
    server_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((server_cfg, resolver))
}

/// SNI-aware cert resolver. Holds an exact-match table for full host
/// names and a wildcard table for `*.example.com`-style patterns.
/// Falls back to the first registered cert when SNI is absent or no
/// pattern matches — matching nginx's default-server behaviour.
///
/// The cert table itself is wrapped in [`arc_swap::ArcSwap`] so
/// SIGHUP can hot-reload certs without restarting the server. The
/// active rustls `ServerConfig` holds an `Arc<dyn ResolvesServerCert>`
/// pointing at this struct; subsequent `reload()` calls atomically
/// swap the inner table, and the next handshake's
/// [`ResolvesServerCert::resolve`] picks the new entry.
#[derive(Debug)]
pub struct MultiCertResolver {
    inner: arc_swap::ArcSwap<CertTable>,
}

/// One snapshot of the cert table. Cheap to construct (cert / key
/// PEMs are read fresh each time) and cheap to swap.
#[derive(Debug)]
struct CertTable {
    /// Lower-cased SNI → `CertifiedKey` for exact-match patterns.
    exact: std::collections::HashMap<String, Arc<rustls::sign::CertifiedKey>>,
    /// `(suffix, ck)` for wildcard patterns. `suffix` is the part after
    /// `*.` (e.g. `example.com` for `*.example.com`), pre-lowercased.
    wildcard: Vec<(String, Arc<rustls::sign::CertifiedKey>)>,
    /// First cert declared in the config; used when SNI is absent or
    /// when nothing in the tables matches. Mirrors nginx's
    /// `default_server` behaviour.
    fallback: Arc<rustls::sign::CertifiedKey>,
}

impl MultiCertResolver {
    /// Build a fresh resolver from the config. Reads + parses every
    /// cert+key referenced in `specs`.
    pub fn from_config(specs: &[conduit_config::CertSpec]) -> Result<Self, TlsError> {
        let table = build_table(specs)?;
        Ok(Self {
            inner: arc_swap::ArcSwap::new(Arc::new(table)),
        })
    }

    /// Atomically swap the cert table. Reads every cert+key from
    /// disk; on failure the previous table is left active and the
    /// error is surfaced (charter rule: a bad reload must not break
    /// a running proxy).
    pub fn reload(&self, specs: &[conduit_config::CertSpec]) -> Result<(), TlsError> {
        let table = build_table(specs)?;
        self.inner.store(Arc::new(table));
        Ok(())
    }

    fn lookup(&self, sni: &str) -> Arc<rustls::sign::CertifiedKey> {
        let table = self.inner.load();
        let key = sni.to_ascii_lowercase();
        if let Some(ck) = table.exact.get(&key) {
            return Arc::clone(ck);
        }
        // `*.foo.com` matches `bar.foo.com` (one label below). Reject
        // matches with multiple labels (no `*.foo.com` for `x.y.foo.com`).
        if let Some(dot_idx) = key.find('.') {
            let suffix = &key[dot_idx + 1..];
            for (pat, ck) in &table.wildcard {
                if pat == suffix {
                    return Arc::clone(ck);
                }
            }
        }
        Arc::clone(&table.fallback)
    }
}

fn build_table(specs: &[conduit_config::CertSpec]) -> Result<CertTable, TlsError> {
    if specs.is_empty() {
        return Err(TlsError::NoCerts);
    }
    let mut exact = std::collections::HashMap::with_capacity(specs.len());
    let mut wildcard = Vec::new();
    let mut fallback: Option<Arc<rustls::sign::CertifiedKey>> = None;
    for spec in specs {
        let certs = load_certs(&spec.cert)?;
        let key = load_key(&spec.key)?;
        let signing_key =
            rustls::crypto::ring::sign::any_supported_type(&key).map_err(|source| {
                TlsError::Rustls {
                    sni: spec.sni.clone(),
                    source,
                }
            })?;
        let ck = Arc::new(rustls::sign::CertifiedKey::new(certs, signing_key));
        if fallback.is_none() {
            fallback = Some(Arc::clone(&ck));
        }
        if let Some(suffix) = spec.sni.strip_prefix("*.") {
            wildcard.push((suffix.to_ascii_lowercase(), ck));
        } else {
            exact.insert(spec.sni.to_ascii_lowercase(), ck);
        }
    }
    Ok(CertTable {
        exact,
        wildcard,
        fallback: fallback.expect("specs non-empty was checked above"),
    })
}

impl rustls::server::ResolvesServerCert for MultiCertResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let sni = client_hello.server_name().unwrap_or("");
        if sni.is_empty() {
            // No SNI → fall back to the first cert.
            Some(Arc::clone(&self.inner.load().fallback))
        } else {
            Some(self.lookup(sni))
        }
    }
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

    /// Build a `MultiCertResolver` directly from in-memory `CertifiedKey`s
    /// for testing the matching logic without writing PEM files to disk.
    fn fake_resolver(entries: Vec<(&str, Arc<rustls::sign::CertifiedKey>)>) -> MultiCertResolver {
        let mut exact = std::collections::HashMap::new();
        let mut wildcard = Vec::new();
        let fallback = Arc::clone(&entries[0].1);
        for (sni, ck) in entries {
            if let Some(suffix) = sni.strip_prefix("*.") {
                wildcard.push((suffix.to_ascii_lowercase(), ck));
            } else {
                exact.insert(sni.to_ascii_lowercase(), ck);
            }
        }
        MultiCertResolver {
            inner: arc_swap::ArcSwap::new(Arc::new(CertTable {
                exact,
                wildcard,
                fallback,
            })),
        }
    }

    /// Generate a fresh self-signed cert+key with rcgen and wrap it in
    /// a `CertifiedKey`. Each call produces a unique `Arc` so pointer
    /// identity (`Arc::ptr_eq`) tells us which entry the resolver
    /// picked.
    fn ck_token() -> Arc<rustls::sign::CertifiedKey> {
        let cert = rcgen::generate_simple_self_signed(vec!["conduit.test".into()])
            .expect("rcgen self-signed");
        let cert_der = cert.cert.der().clone();
        let key_der_bytes = cert.signing_key.serialize_der();
        let key_der = rustls_pki_types::PrivatePkcs8KeyDer::from(key_der_bytes);
        let key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(key_der))
            .expect("ring accepts rcgen-generated key");
        Arc::new(rustls::sign::CertifiedKey::new(vec![cert_der], key))
    }

    #[test]
    fn resolver_exact_match_is_case_insensitive() {
        let exam = ck_token();
        let other = ck_token();
        let r = fake_resolver(vec![
            ("example.com", Arc::clone(&exam)),
            ("api.example.com", Arc::clone(&other)),
        ]);
        assert!(Arc::ptr_eq(&r.lookup("example.com"), &exam));
        assert!(Arc::ptr_eq(&r.lookup("EXAMPLE.com"), &exam));
        assert!(Arc::ptr_eq(&r.lookup("api.example.com"), &other));
    }

    #[test]
    fn resolver_wildcard_matches_one_label() {
        let wild = ck_token();
        let r = fake_resolver(vec![("*.example.com", Arc::clone(&wild))]);
        assert!(Arc::ptr_eq(&r.lookup("api.example.com"), &wild));
        assert!(Arc::ptr_eq(&r.lookup("WWW.example.com"), &wild));
        // Two labels below: should NOT match the wildcard; falls back.
        // Fallback is the first entry, which is `wild` here, so we'd
        // get `wild` either way — assert via a non-matching peer.
        let other = ck_token();
        let r = fake_resolver(vec![
            ("default.example.com", Arc::clone(&other)),
            ("*.example.com", Arc::clone(&wild)),
        ]);
        // a.b.example.com is two labels below; wildcard does not match.
        // No exact match either; resolver falls back to first entry.
        assert!(Arc::ptr_eq(&r.lookup("a.b.example.com"), &other));
    }

    #[test]
    fn resolver_falls_back_to_first_when_nothing_matches() {
        let first = ck_token();
        let second = ck_token();
        let r = fake_resolver(vec![
            ("api.example.com", Arc::clone(&first)),
            ("*.other.com", Arc::clone(&second)),
        ]);
        assert!(Arc::ptr_eq(&r.lookup("unknown.host"), &first));
    }

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
