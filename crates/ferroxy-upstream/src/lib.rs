//! ferroxy-upstream — connection pool keyed by (authority, protocol, tls
//! params), active+passive health checks, circuit breaker, retry policy.
//! Owns upstream connection lifetime.
//!
//! Implemented in phase 4 of the build order (h1 first), extended in phases
//! 7 and 9 for h2 and h3 upstreams.
#![deny(missing_docs)]
