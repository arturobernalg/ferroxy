//! conduit-transport — Layer 2: TLS termination via rustls.
//!
//! # What this crate does today
//!
//! Two cold-path entry points:
//!
//! - [`load_server_config`] reads cert + key files from a config
//!   document and builds a `rustls::ServerConfig`. Errors surface
//!   the path at fault.
//! - [`accept_tls`] wraps a tokio `TcpStream` in a TLS handshake and
//!   returns the encrypted stream. The result is a tokio
//!   `AsyncRead + AsyncWrite` that `conduit-h1::serve_connection`
//!   consumes unchanged.
//!
//! # What this crate does not do (yet)
//!
//! - **SNI multi-cert**. P6 ships single-cert termination only:
//!   the *first* cert in `[tls].certs` is used, and the rest are
//!   ignored with a warning. SNI-based cert selection is the next
//!   cleanup pass.
//! - **Hot-reload via `arc-swap`**. Once SNI is wired, the cert
//!   table becomes the natural unit of hot-reload.
//! - **OCSP stapling**, **session tickets**, **session-ID cache**.
//!   Defaults are whatever rustls picks today; per-charter knobs
//!   plug in once we have a working bench harness to measure
//!   their effect.
//! - **mTLS to upstream** is owned by `conduit-upstream`; this crate
//!   only does ingress termination.
//! - **QUIC + ALPN dispatch** for HTTP/3 lands with P9.
//!
//! All deferrals are tracked in `phase6_deviations` in project memory.

#![deny(missing_docs)]

mod tls;

pub use tls::{accept_tls, build_acceptor, load_server_config, TlsAcceptor, TlsError};
