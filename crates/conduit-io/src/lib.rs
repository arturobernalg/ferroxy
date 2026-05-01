//! conduit-io — Layer 1: monoio listeners, accept loop, thread-per-core
//! worker model with `SO_REUSEPORT` and CPU pinning.
//!
//! # Concurrency model
//!
//! `serve(spec, setup)` spawns `spec.workers` OS threads. Each thread:
//!   - (optionally) pins itself to a CPU core,
//!   - builds a single-thread monoio `io_uring` runtime,
//!   - takes its slice of pre-bound `SO_REUSEPORT` listeners,
//!   - calls `setup()` to instantiate a per-worker handler `H`,
//!   - runs an accept loop per listener on its runtime.
//!
//! `setup` runs *inside* the worker thread, so `H` can capture per-worker
//! state (`Rc<RefCell<…>>` etc.) that is `!Send`. Connection futures
//! likewise do not need `Send` — they run on the same single-thread
//! runtime that accepted them.
//!
//! # Shutdown
//!
//! [`Server::shutdown_trigger`] returns a cheaply cloneable
//! [`ShutdownTrigger`]. Calling [`ShutdownTrigger::cancel`] sets a shared
//! `Arc<AtomicBool>`; workers poll it at `spec.poll_interval` (default
//! 50 ms). Accept loops then exit and the worker drains in-flight
//! connection tasks until they finish naturally or `shutdown_deadline`
//! elapses (in which case the monoio runtime drop aborts them).
//!
//! # Invariants
//!
//! - Bind happens entirely on the calling thread before any worker is
//!   spawned. Any failure closes every listener already opened in this
//!   call (Vec drop) and returns `BindError`.
//! - `port = 0` is resolved on the first bind and the resolved port is
//!   used for the remaining `(workers - 1)` `SO_REUSEPORT` binds, so the
//!   cluster shares one address.
//! - [`Server::wait`] is **synchronous**: it joins the OS threads and
//!   sums their counters. The binary needs no async runtime to run the
//!   server.
//! - `accepted == completed` after a clean drain. If the deadline
//!   elapses before equalisation, the report still reports the running
//!   counters; the gap is the population that was aborted on runtime
//!   drop.

#![deny(missing_docs)]

mod bind;
mod worker;

use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use monoio::net::TcpStream;

// Re-export for callers that want to write `conduit_io::TcpStream`.
pub use monoio::net::TcpStream as MonoioTcpStream;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Knobs for [`serve`]. Cold path; cloned at most once per `serve` call.
#[derive(Debug, Clone)]
pub struct ServeSpec {
    /// Addresses to bind. Must be non-empty.
    pub listeners: Vec<SocketAddr>,
    /// Worker count.
    pub workers: NonZeroUsize,
    /// Pin worker N to core N when at least N cores are reported by
    /// [`core_affinity::get_core_ids`].
    pub cpu_affinity: bool,
    /// Per-worker drain budget after shutdown is signalled.
    pub shutdown_deadline: Duration,
    /// `TCP_NODELAY` on accepted streams.
    pub tcp_nodelay: bool,
    /// `listen(2)` backlog applied per listener.
    pub backlog: u32,
    /// `io_uring` submission queue depth per worker.
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
}

/// Cheaply cloneable shutdown handle. All clones share one
/// `Arc<AtomicBool>`; calling [`ShutdownTrigger::cancel`] is idempotent.
#[derive(Clone)]
pub struct ShutdownTrigger {
    flag: Arc<AtomicBool>,
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
    /// Idempotent: subsequent calls are no-ops.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// Aggregate counts from a graceful shutdown.
///
/// `accepted >= completed` always. After a clean drain, equality holds.
/// A gap means the per-worker drain deadline elapsed and the remaining
/// tasks were aborted by the runtime drop.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShutdownReport {
    /// Total connections accepted across all workers.
    pub accepted: u64,
    /// Connection handlers that returned cleanly.
    pub completed: u64,
    /// Wall time from [`serve`] returning to [`Server::wait`] returning.
    pub elapsed: Duration,
}

/// Active server: owns every worker OS thread + the shared shutdown flag.
///
/// Single-shot: consumed by [`Server::wait`].
#[derive(Debug)]
pub struct Server {
    threads: Vec<thread::JoinHandle<worker::WorkerCounts>>,
    flag: Arc<AtomicBool>,
    bound: Vec<SocketAddr>,
    started: Instant,
}

impl Server {
    /// Cheaply cloneable handle for triggering shutdown from any thread.
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger {
            flag: Arc::clone(&self.flag),
        }
    }

    /// The actual addresses every listener is bound to. `port = 0` is
    /// resolved to the OS-assigned port. One entry per
    /// `ServeSpec.listeners` entry.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.bound
    }

    /// Block until every worker thread joins. Sums the per-worker
    /// counters into [`ShutdownReport`]. Synchronous: no tokio/monoio
    /// runtime required in the caller's context.
    pub fn wait(self) -> ShutdownReport {
        let started = self.started;
        let mut report = ShutdownReport::default();
        for h in self.threads {
            match h.join() {
                Ok(c) => {
                    report.accepted += c.accepted;
                    report.completed += c.completed;
                }
                Err(_) => {
                    tracing::error!("worker thread panicked");
                }
            }
        }
        report.elapsed = started.elapsed();
        report
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

/// Bind every (worker × addr) socket and start accepting.
///
/// `setup` is a per-worker handler factory. It runs inside the worker
/// thread once, *after* CPU pinning and the monoio runtime are in place,
/// and produces the connection handler `H` for that worker. `H` may
/// capture `!Send` per-worker state.
///
/// On any bind failure, every listener already opened in this call is
/// closed before returning; the caller's process is unchanged.
//
// `spec` is taken by value at the public surface: `serve` is the
// "hand-off to a long-running subsystem" entry point, and asking every
// caller for `&spec` to please clippy buys nothing.
#[allow(clippy::needless_pass_by_value)]
pub fn serve<S, H, Fut>(spec: ServeSpec, setup: S) -> Result<Server, BindError>
where
    S: Fn() -> H + Send + Sync + 'static,
    H: Fn(TcpStream, SocketAddr) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    if spec.listeners.is_empty() {
        return Err(BindError::NoListeners);
    }

    let n = spec.workers.get();

    // Bind every (worker × addr) socket on this thread before spawning
    // workers, so a port-in-use failure surfaces here and any sockets
    // already opened are closed by the Vec drop on early return.
    //
    // For each requested address, the *first* bind also resolves
    // `port = 0` to the OS-assigned port so the remaining
    // `(workers - 1)` binds share that same port via `SO_REUSEPORT`.
    let mut per_worker: Vec<Vec<(SocketAddr, std::net::TcpListener)>> = (0..n)
        .map(|_| Vec::with_capacity(spec.listeners.len()))
        .collect();
    let mut bound: Vec<SocketAddr> = Vec::with_capacity(spec.listeners.len());

    for listener_addr in &spec.listeners {
        let first =
            bind::bind(*listener_addr, spec.backlog, true).map_err(|source| BindError::Bind {
                addr: *listener_addr,
                source,
            })?;
        let resolved = first.local_addr().map_err(|source| BindError::Bind {
            addr: *listener_addr,
            source,
        })?;
        bound.push(resolved);
        per_worker[0].push((resolved, first));
        for slot in per_worker.iter_mut().skip(1) {
            let l = bind::bind(resolved, spec.backlog, true).map_err(|source| BindError::Bind {
                addr: resolved,
                source,
            })?;
            slot.push((resolved, l));
        }
    }

    let flag = Arc::new(AtomicBool::new(false));
    let setup = Arc::new(setup);
    let cores = if spec.cpu_affinity {
        core_affinity::get_core_ids().unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut threads: Vec<thread::JoinHandle<worker::WorkerCounts>> = Vec::with_capacity(n);
    for (idx, listeners) in per_worker.into_iter().enumerate() {
        let flag = Arc::clone(&flag);
        let setup = Arc::clone(&setup);
        let core = cores.get(idx).copied();
        let uring = spec.uring_entries;
        let deadline = spec.shutdown_deadline;
        let nodelay = spec.tcp_nodelay;
        let poll = spec.poll_interval;

        let th = thread::Builder::new()
            .name(format!("conduit-io-w{idx}"))
            .spawn(move || {
                // setup runs in the new thread so H can capture !Send state.
                let handler = (*setup)();
                worker::run(
                    listeners, handler, &flag, uring, deadline, nodelay, poll, core,
                )
            })
            .map_err(BindError::ThreadSpawn)?;
        threads.push(th);
    }

    Ok(Server {
        threads,
        flag,
        bound,
        started: Instant::now(),
    })
}
