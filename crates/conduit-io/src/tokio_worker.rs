//! Tokio backend with per-worker sharding.
//!
//! Production runtime on non-Linux hosts (where monoio is unavailable)
//! and the comparison runtime under `--features runtime-tokio` on
//! Linux for the Phase 11 bench harness.
//!
//! # Design
//!
//! N OS threads, one per worker, each running its own current-thread
//! tokio runtime. Listeners are bound N times per address with
//! `SO_REUSEPORT` so the kernel distributes incoming connections
//! across workers. Each worker calls `setup()` once per listener to
//! get its own handler instance, so any per-worker state (e.g. a
//! per-worker connection-pool shard) lives entirely on that thread —
//! no Mutex contention across workers on the hot path.
//!
//! This shape matches the monoio backend exactly. The two backends
//! differ only in:
//!   - the runtime builder used (`new_current_thread()` vs monoio's)
//!   - the listener type (`tokio::net::TcpListener` vs `monoio::net::TcpListener`)
//!   - per-worker counters (`AtomicU64` here, `Cell<u64>` on monoio)
//!
//! Connection handler bound is identical to the monoio backend's
//! shape (`Fn() -> Fn(TcpStream, SocketAddr) -> Future`) so the
//! binary needs no `cfg` flag at the call site beyond the type alias
//! re-export of `TcpStream`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder as RuntimeBuilder;

use crate::bind;
use crate::{BindError, ServeSpec, ShutdownReport, ShutdownTrigger};

/// Active server: owns the worker threads + the shared shutdown flag.
///
/// Single-shot: consumed by [`Server::wait`].
#[derive(Debug)]
pub struct Server {
    worker_threads: Vec<thread::JoinHandle<WorkerCounts>>,
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
    /// resolved to the OS-assigned port.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.bound
    }

    /// Block until every worker thread joins. Returns aggregate counts.
    /// Synchronous: no async runtime required in the caller's context.
    pub fn wait(mut self) -> ShutdownReport {
        let started = self.started;
        let mut totals = WorkerCounts::default();
        for h in self.worker_threads.drain(..) {
            let c = h.join().unwrap_or_default();
            totals.accepted = totals.accepted.saturating_add(c.accepted);
            totals.completed = totals.completed.saturating_add(c.completed);
        }
        ShutdownReport {
            accepted: totals.accepted,
            completed: totals.completed,
            elapsed: started.elapsed(),
        }
    }
}

/// What each worker thread hands back when it joins.
#[derive(Debug, Default, Clone, Copy)]
struct WorkerCounts {
    accepted: u64,
    completed: u64,
}

/// Bind every listener (N times per address with `SO_REUSEPORT`) and
/// start one worker thread per shard.
///
/// Each worker calls `setup()` once per listener to build its own
/// handler. Per-worker state (e.g. a connection-pool shard) lives
/// entirely on that thread — no Mutex contention across workers on
/// the hot path.
//
// `spec` is taken by value at the public surface for the same reason
// as the monoio backend: callers expect a hand-off shape.
#[allow(clippy::needless_pass_by_value)]
pub fn serve<S, H, Fut>(spec: ServeSpec, setup: S) -> Result<Server, BindError>
where
    S: Fn() -> H + Send + Sync + 'static,
    H: Fn(TcpStream, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if spec.listeners.is_empty() {
        return Err(BindError::NoListeners);
    }

    let n = spec.workers.get();
    // N×L bindings: one per (worker, addr) pair. The first bind for
    // an addr resolves port=0 to a concrete port; subsequent binds
    // for the same addr use the resolved port so SO_REUSEPORT can
    // pair them up.
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

    let deadline = spec.shutdown_deadline;
    let nodelay = spec.tcp_nodelay;
    let poll_interval = spec.poll_interval;

    let mut worker_threads: Vec<thread::JoinHandle<WorkerCounts>> = Vec::with_capacity(n);
    for (idx, listeners) in per_worker.into_iter().enumerate() {
        let flag = Arc::clone(&flag);
        let setup = Arc::clone(&setup);
        let handle = thread::Builder::new()
            .name(format!("conduit-io-tokio-w{idx}"))
            .spawn(move || {
                let rt = match RuntimeBuilder::new_current_thread().enable_all().build() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            worker = idx,
                            "failed to build tokio runtime",
                        );
                        return WorkerCounts::default();
                    }
                };
                rt.block_on(async move {
                    run_worker(listeners, setup, flag, nodelay, deadline, poll_interval).await
                })
            })
            .map_err(BindError::ThreadSpawn)?;
        worker_threads.push(handle);
    }

    Ok(Server {
        worker_threads,
        flag,
        bound,
        started: Instant::now(),
    })
}

async fn run_worker<S, H, Fut>(
    listeners: Vec<(SocketAddr, std::net::TcpListener)>,
    setup: Arc<S>,
    flag: Arc<AtomicBool>,
    tcp_nodelay: bool,
    deadline: Duration,
    poll_interval: Duration,
) -> WorkerCounts
where
    S: Fn() -> H + Send + Sync + 'static,
    H: Fn(TcpStream, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let counts = Arc::new(CountsAtomic::default());
    let mut tasks = tokio::task::JoinSet::new();

    for (addr, std_l) in listeners {
        let l = match TcpListener::from_std(std_l) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %addr,
                    "tokio listener conversion failed; this listener is skipped",
                );
                continue;
            }
        };
        // setup is called once per listener per worker — the handler
        // captures per-worker state.
        let handler = setup();
        let counts = Arc::clone(&counts);
        let flag = Arc::clone(&flag);
        tasks.spawn(accept_loop(
            l,
            handler,
            counts,
            flag,
            tcp_nodelay,
            poll_interval,
        ));
    }

    while tasks.join_next().await.is_some() {}

    // Drain phase: wait for in-flight conn tasks (spawned via
    // tokio::spawn from inside accept_loop) to finish or for the
    // deadline to elapse. We rely on the count invariant
    // `accepted == completed` once everything has drained.
    let drain_start = Instant::now();
    while counts.accepted.load(Ordering::Acquire) != counts.completed.load(Ordering::Acquire) {
        if drain_start.elapsed() >= deadline {
            tracing::warn!(
                accepted = counts.accepted.load(Ordering::Acquire),
                completed = counts.completed.load(Ordering::Acquire),
                "shutdown deadline elapsed; in-flight tasks aborted on runtime drop",
            );
            break;
        }
        tokio::time::sleep(poll_interval).await;
    }

    WorkerCounts {
        accepted: counts.accepted.load(Ordering::Acquire),
        completed: counts.completed.load(Ordering::Acquire),
    }
}

/// Per-worker counters. Atomic so the accept loop and the spawned
/// connection tasks can both update them without coordination —
/// they share an `Arc<CountsAtomic>` per worker but the atomics
/// themselves never see cross-worker traffic.
#[derive(Default)]
struct CountsAtomic {
    accepted: AtomicU64,
    completed: AtomicU64,
}

async fn accept_loop<H, Fut>(
    listener: TcpListener,
    handler: H,
    counts: Arc<CountsAtomic>,
    flag: Arc<AtomicBool>,
    tcp_nodelay: bool,
    poll_interval: Duration,
) where
    H: Fn(TcpStream, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let handler = Arc::new(handler);
    loop {
        if flag.load(Ordering::Acquire) {
            break;
        }
        match tokio::time::timeout(poll_interval, listener.accept()).await {
            Ok(Ok((stream, peer))) => {
                counts.accepted.fetch_add(1, Ordering::AcqRel);
                if tcp_nodelay {
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::warn!(error = %e, "set_nodelay failed");
                    }
                }
                let h = Arc::clone(&handler);
                let c = Arc::clone(&counts);
                tokio::spawn(async move {
                    h(stream, peer).await;
                    c.completed.fetch_add(1, Ordering::AcqRel);
                });
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "accept error");
            }
            Err(_elapsed) => {
                // fall through to next-iter flag check
            }
        }
    }
}
