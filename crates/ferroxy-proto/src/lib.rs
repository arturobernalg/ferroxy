//! ferroxy-proto — the single shared request/response stream contract.
//!
//! Per the engineering charter this is the **one** real abstraction in the
//! system. It must model exactly head, streaming body, trailers,
//! cancellation, and half-close — and nothing more. Each protocol crate
//! (`ferroxy-h1`, `ferroxy-h2`, `ferroxy-h3`) maps wire format to this type;
//! `ferroxy-lifecycle` reads from and writes back to this type.
//!
//! Implemented in phase 2 of the build order.
#![deny(missing_docs)]
