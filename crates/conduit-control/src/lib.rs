//! conduit-control — Layer 6: admin endpoints + (forthcoming) hot-reload,
//! metrics, OpenTelemetry tracing, shutdown coordination.
//!
//! # What this crate does today
//!
//! - [`serve_admin`] runs an HTTP/1.1 admin server on a tokio
//!   `TcpListener`. The server recognises:
//!     - `GET /health` — liveness probe; always 200 OK as long as the
//!       process is up.
//!     - `GET /ready` — readiness probe; 200 OK once the proxy has
//!       finished startup. (Currently always ready since startup is
//!       synchronous; will track upstream health in P10 proper.)
//! - Cancellation: `serve_admin` accepts a future that resolves when
//!   the caller wants to stop accepting; on completion it stops
//!   accepting and returns. In-flight admin requests are short and
//!   are not tracked.
//!
//! # What this crate does not do (yet)
//!
//! - SIGHUP-driven hot-reload of the config + atomic swap of the
//!   live `Dispatch`.
//! - `/config` (read-only effective TOML), `/pools` (live pool
//!   stats), `/reload` (POST trigger).
//! - Prometheus `/metrics` endpoint with histogram + counter family
//!   per the engineering charter.
//! - OpenTelemetry tracing wiring with W3C `traceparent` propagation.
//!
//! All of these are P10 deliverables; this crate stubs the surface
//! and ships the two operational must-haves (`/health`, `/ready`) so
//! the binary can pass kubernetes-style liveness/readiness probes
//! today.

#![deny(missing_docs)]

mod admin;

pub use admin::{serve_admin, AdminError};
