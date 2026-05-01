//! conduit-io — Layer 1: listeners, accept loop, thread-per-core worker
//! model.
//!
//! # Backend selection
//!
//! - `target_os = "linux"`, no `runtime-tokio` feature: **monoio**
//!   (`io_uring`, thread-per-core, share-nothing). Production backend.
//! - Otherwise (non-Linux dev hosts, or Linux with explicit
//!   `--features runtime-tokio` for benchmark comparison): **tokio**
//!   stub. The public API compiles; `serve` returns
//!   [`BindError::TokioBackendUnimplemented`] at runtime. The full
//!   tokio implementation lands in Phase 11 alongside the
//!   nginx/Pingora bench harness.
//!
//! See `Supported Platforms` in the project README for the policy.

#![deny(missing_docs)]

mod bind;

#[cfg(all(target_os = "linux", not(feature = "runtime-tokio")))]
mod monoio_worker;
#[cfg(any(not(target_os = "linux"), feature = "runtime-tokio"))]
mod tokio_worker;

use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Public types — runtime-agnostic, defined here once.
// ---------------------------------------------------------------------------

/// Knobs for [`serve`]. Cold path; cloned at most once per `serve` call.
#[derive(Debug, Clone)]
pub struct ServeSpec {
    /// Addresses to bind. Must be non-empty.
    pub listeners: Vec<SocketAddr>,
    /// Worker count.
    pub workers: NonZeroUsize,
    /// Pin worker N to core N when at least N cores are reported by the
    /// platform's affinity API. Silently ignored on the tokio dev
    /// backend.
    pub cpu_affinity: bool,
    /// Per-worker drain budget after shutdown is signalled.
    pub shutdown_deadline: Duration,
    /// `TCP_NODELAY` on accepted streams.
    pub tcp_nodelay: bool,
    /// `listen(2)` backlog applied per listener.
    pub backlog: u32,
    /// `io_uring` submission queue depth per worker. Ignored on the
    /// tokio backend (no `io_uring`).
    pub uring_entries: u32,
    /// Cadence at which workers poll the shutdown flag.
    pub poll_interval: Duration,
}

impl ServeSpec {
    /// Build a spec with sensible defaults: 30s drain, `TCP_NODELAY`,
    /// 1024 backlog, 1024 `io_uring` entries, 50 ms shutdown poll.
    pub fn new(listeners: Vec<SocketAddr>, workers: NonZeroUsize) -> Self {
        Self {
            listeners,
            workers,
            cpu_affinity: true,
            shutdown_deadline: Duration::from_secs(30),
            tcp_nodelay: true,
            backlog: 1024,
            uring_entries: 1024,
            poll_interval: Duration::from_millis(50),
        }
    }
}

/// Errors returned by [`serve`] before any connection is accepted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BindError {
    /// `serve` was called with an empty `listeners` list.
    #[error("listener list is empty")]
    NoListeners,

    /// `bind(2)` or `listen(2)` failed for `addr`.
    #[error("bind failed for {addr}")]
    Bind {
        /// The address that could not be bound.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// `std::thread::Builder::spawn` failed for a worker thread.
    #[error("worker thread spawn failed")]
    ThreadSpawn(#[source] io::Error),

    /// The tokio dev backend was selected but is not yet implemented.
    /// Use Linux for production deployments; the tokio path lands in
    /// Phase 11 alongside the bench harness.
    #[error(
        "tokio runtime backend is not yet implemented; build for Linux for production use, \
         or wait for Phase 11"
    )]
    TokioBackendUnimplemented,
}

/// Cheaply cloneable shutdown handle.
#[derive(Clone)]
pub struct ShutdownTrigger {
    pub(crate) flag: Arc<AtomicBool>,
}

impl std::fmt::Debug for ShutdownTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownTrigger")
            .field("triggered", &self.flag.load(Ordering::Acquire))
            .finish()
    }
}

impl ShutdownTrigger {
    /// Signal every worker to stop accepting and begin draining.
    /// Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// Aggregate counts from a graceful shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShutdownReport {
    /// Total connections accepted across all workers.
    pub accepted: u64,
    /// Connection handlers that returned cleanly.
    pub completed: u64,
    /// Wall time from [`serve`] returning to [`Server::wait`] returning.
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// Backend re-exports.
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", not(feature = "runtime-tokio")))]
pub use monoio::net::TcpStream;
#[cfg(all(target_os = "linux", not(feature = "runtime-tokio")))]
pub use monoio_worker::{serve, Server};

#[cfg(any(not(target_os = "linux"), feature = "runtime-tokio"))]
pub use tokio::net::TcpStream;
#[cfg(any(not(target_os = "linux"), feature = "runtime-tokio"))]
pub use tokio_worker::{serve, Server};
