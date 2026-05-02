//! conduit-transport — Layer 2: TLS termination via rustls.
//!
//! # What this crate does today
//!
//! - [`load_server_config`] reads cert + key files from a config
//!   document and builds a `rustls::ServerConfig` with SNI
//!   multi-cert resolution + ALPN advertising `h2,http/1.1`.
//! - [`load_server_config_with_resolver`] returns the same plus an
//!   `Arc<MultiCertResolver>` handle so the caller can call
//!   [`MultiCertResolver::reload`] on SIGHUP to hot-swap certs
//!   without restarting the listener.
//! - [`accept_tls`] wraps a tokio `TcpStream` in a TLS handshake and
//!   returns the encrypted stream.
//!
//! # What this crate does not do (yet)
//!
//! - **OCSP stapling**, **session tickets**, **session-ID cache**.
//!   Defaults are whatever rustls picks today; per-charter knobs
//!   plug in once we have a working bench harness to measure
//!   their effect.
//! - **mTLS to upstream** is owned by `conduit-upstream`; this crate
//!   only does ingress termination.
//!
//! Deferrals are tracked in `phase6_deviations` in project memory.

#![deny(missing_docs)]

mod tls;

pub use tls::{
    accept_tls, build_acceptor, load_server_config, load_server_config_with_resolver,
    MultiCertResolver, TlsAcceptor, TlsError,
};
