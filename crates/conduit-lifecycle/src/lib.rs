//! conduit-lifecycle — Layer 4: owns the request from handed-off-by-protocol
//! to handed-back-to-protocol. Routing, filter chain, upstream selection,
//! body piping, timeout enforcement, trailer propagation.
//!
//! Implemented in phase 5 of the build order.
#![deny(missing_docs)]
