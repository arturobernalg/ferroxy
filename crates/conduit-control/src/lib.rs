//! conduit-control — Layer 6: admin endpoints, metrics, hot-reload
//! coordination, observability surfaces.
//!
//! # What this crate does today
//!
//! - [`serve_admin`] runs an HTTP/1.1 admin server on a tokio
//!   `TcpListener`. The server recognises:
//!     - `GET /health` — liveness probe; always 200 OK as long as the
//!       process is up.
//!     - `GET /ready` — readiness probe; 200 OK once the proxy has
//!       finished startup. (Currently always ready since startup is
//!       synchronous; will track upstream health when active health
//!       checks ship.)
//!     - `GET /metrics` — Prometheus 0.0.4 text-format exposition.
//!       The data-plane-fed counter family ([`MetricsHandle`]) lives
//!       beside the synthetic `conduit_uptime_seconds` /
//!       `conduit_build_info` gauges.
//! - [`MetricsHandle`] — lock-free atomic counters fed by the data
//!   plane (one shared `Arc<MetricsHandle>` across every per-request
//!   handler). The admin server holds the same handle and reads it
//!   on every `/metrics` scrape.
//! - Cancellation: `serve_admin` accepts an `AtomicBool` flag the
//!   caller flips on shutdown; the accept loop polls it between
//!   accepts. In-flight admin requests are short and are not tracked.
//!
//! # What this crate does not do (yet)
//!
//! - `/config` (read-only effective TOML), `/pools` (live pool
//!   stats), `/reload` (POST-triggered reload). SIGHUP-triggered
//!   reload is wired in the binary today; an HTTP trigger is P10.x.
//! - Histogram families for request latency. Counters ship now;
//!   histograms wait on a hdrhistogram-backed shared sink.
//! - OpenTelemetry tracing wiring with W3C `traceparent` propagation.

#![deny(missing_docs)]

mod admin;
mod metrics;

pub use admin::{serve_admin, AdminError};
pub use metrics::MetricsHandle;
