//! conduit-lifecycle — layer 4: matches an incoming request against
//! the configured routes, selects an upstream, and forwards.
//!
//! # What this crate does
//!
//! Two cold-path types build at config load:
//!
//! - [`UpstreamMap`] maps an upstream *name* to a live
//!   [`conduit_upstream::Upstream`].
//! - [`RouteTable`] maps an incoming request (Host header + path) to
//!   an upstream name.
//!
//! One hot-path entry point:
//!
//! - [`Dispatch::handle`] takes an incoming request, looks up the
//!   matching route, picks the named upstream, and runs it through
//!   the upstream's `forward`. Returns a response (or a 502/503/504
//!   on failure).
//!
//! # What this crate does not do (yet)
//!
//! - **Prefix-trie route table.** P5 ships with a linear scan; for
//!   100 routes that's fine, and the trie can replace the matcher
//!   without changing the public surface once profiling motivates it
//!   (P11.5).
//! - **Regex path matching.** `path_regex` config field parses but
//!   isn't yet evaluated. Surfacing as a P5.x deviation.
//! - **Filter chain.** The `filters = [...]` block parses (P0) but
//!   filter execution lands in P5.x — the API surface here is shaped
//!   to take filters as a layer between match and dispatch.
//! - **Timeout enforcement** beyond what the upstream client gives
//!   us. Per-route connect/read/write/total timeouts plumb through
//!   in P5.x.
//! - **Header rewriting** (`X-Forwarded-For`, hop-by-hop stripping
//!   per RFC 9110 §7.6.1). P5.x — straightforward but mechanical.
//!
//! All deferrals are tracked in `phase5_deviations` in project memory.

#![deny(missing_docs)]

mod table;

pub use table::{Dispatch, DispatchError, PathMatch, Route, RouteTable, UpstreamMap};
