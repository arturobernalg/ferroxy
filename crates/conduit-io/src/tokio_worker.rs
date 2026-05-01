//! Tokio backend.
//!
//! Production runtime on non-Linux hosts (where monoio is unavailable)
//! and the comparison runtime under `--features runtime-tokio` on
//! Linux for the Phase 11 bench harness.
//!
//! # Design
//!
//! `serve` builds a multi-thread tokio runtime sized to
//! `spec.workers` on a dedicated OS thread, binds listeners
//! (synchronously, via the same socket2-based `bind::bind` helper the
//! monoio backend uses), spawns one accept loop per listener, and
//! returns a [`Server`] handle. [`Server::wait`] joins the runtime
//! thread; cancellation flips a shared `Arc<AtomicBool>` that accept
//! loops poll between `accept().await` calls (timeout-cancellation
//! pattern shared with the monoio backend).
//!
//! Connection handler bound is identical to the monoio backend's
//! shape (`Fn() -> Fn(TcpStream, SocketAddr) -> Future`) so the
//! binary needs no `cfg` flag at the call site beyond the type alias
//! re-export of `TcpStream`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder as RuntimeBuilder;

use crate::bind;
use crate::{BindError, ServeSpec, ShutdownReport, ShutdownTrigger};

/// Active server: owns the runtime thread + the shared shutdown flag.
///
/// Single-shot: consumed by [`Server::wait`].
#[derive(Debug)]
pub struct Server {
    runtime_thread: Option<thread::JoinHandle<WorkerCounts>>,
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

    /// Block until the runtime thread joins. Returns aggregate counts.
    /// Synchronous: no async runtime required in the caller's context.
    pub fn wait(mut self) -> ShutdownReport {
        let started = self.started;
        let counts = match self.runtime_thread.take() {
            Some(h) => h.join().unwrap_or_default(),
            None => WorkerCounts::default(),
        };
        ShutdownReport {
            accepted: counts.accepted,
            completed: counts.completed,
            elapsed: started.elapsed(),
        }
    }
}

/// What the runtime thread hands back when it joins.
#[derive(Debug, Default, Clone, Copy)]
struct WorkerCounts {
    accepted: u64,
    completed: u64,
}

/// Bind every listener and start accepting on a dedicated tokio runtime.
///
/// The tokio backend uses a multi-thread runtime, so handler closures
/// must produce `Send` futures — this is the only API difference from
/// the monoio backend. Realistic handlers (e.g. `Dispatch::handle`
/// from `conduit-lifecycle`) satisfy `Send` already because their
/// captured state is `Arc`-wrapped.
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

    // Bind synchronously up front so a port-in-use failure surfaces
    // here, before the runtime thread starts.
    let mut std_listeners: Vec<(SocketAddr, std::net::TcpListener)> =
        Vec::with_capacity(spec.listeners.len());
    for addr in &spec.listeners {
        let l = bind::bind(*addr, spec.backlog, false).map_err(|source| BindError::Bind {
            addr: *addr,
            source,
        })?;
        let resolved = l.local_addr().map_err(|source| BindError::Bind {
            addr: *addr,
            source,
        })?;
        std_listeners.push((resolved, l));
    }
    let bound: Vec<SocketAddr> = std_listeners.iter().map(|(a, _)| *a).collect();

    let flag = Arc::new(AtomicBool::new(false));
    let setup = Arc::new(setup);

    let workers = spec.workers.get();
    let deadline = spec.shutdown_deadline;
    let nodelay = spec.tcp_nodelay;
    let poll_interval = spec.poll_interval;

    let flag_for_thread = Arc::clone(&flag);
    let runtime_thread = thread::Builder::new()
        .name("conduit-io-tokio-rt".into())
        .spawn(move || {
            let rt = match RuntimeBuilder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .thread_name("conduit-io-w")
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "failed to build tokio runtime");
                    return WorkerCounts::default();
                }
            };
            rt.block_on(async move {
                run_accept_loops(
                    std_listeners,
                    setup,
                    flag_for_thread,
                    nodelay,
                    deadline,
                    poll_interval,
                )
                .await
            })
        })
        .map_err(BindError::ThreadSpawn)?;

    Ok(Server {
        runtime_thread: Some(runtime_thread),
        flag,
        bound,
        started: Instant::now(),
    })
}

async fn run_accept_loops<S, H, Fut>(
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
        // setup is called once per accept loop (one per listener) so
        // the handler can capture per-loop state.
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

/// Atomic counts shared across accept loops + connection tasks. The
/// monoio backend uses `Cell<u64>` because per-worker state stays on
/// one thread; tokio is multi-thread, so we go atomic.
#[derive(Default)]
struct CountsAtomic {
    accepted: std::sync::atomic::AtomicU64,
    completed: std::sync::atomic::AtomicU64,
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
