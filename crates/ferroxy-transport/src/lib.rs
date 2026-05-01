//! ferroxy-transport — Layer 2: TLS termination (rustls) and QUIC (quinn).
//!
//! Performs ALPN dispatch and hands the negotiated protocol + stream up to
//! Layer 3 (the protocol crates). Knows nothing about routing.
//!
//! Implemented in phase 6 of the build order.
#![deny(missing_docs)]
