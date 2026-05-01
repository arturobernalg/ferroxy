//! Monoio backend: production runtime for Linux. One worker = one OS
//! thread = one monoio current-thread `io_uring` runtime. `SO_REUSEPORT`
//! shares listeners across workers; the kernel does the load balancing.
//!
//! Per-worker, an accept loop runs for every listen address. Connections
//! are spawned as monoio tasks on the same runtime — never crossing
//! thread boundaries.
//!
//! Shutdown is observed by polling a shared `Arc<AtomicBool>` at
//! `poll_interval`. After cancellation, accept loops exit; the worker
//! drains in-flight connection tasks until either `accepted == completed`
//! or `shutdown_deadline` elapses, at which point the runtime drops and
//! remaining tasks abort.
//!
//! All per-worker state is local: handler counts live in `Rc<Cell<u64>>`
//! to keep the hot path off cross-thread atomics. Cross-thread totals are
//! returned from the OS thread's join handle.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use monoio::net::{TcpListener, TcpStream};

use crate::bind;
use crate::{BindError, ServeSpec, ShutdownReport, ShutdownTrigger};

/// Active server: owns every worker OS thread + the shared shutdown flag.
///
/// Single-shot: consumed by [`Server::wait`].
#[derive(Debug)]
pub struct Server {
    threads: Vec<thread::JoinHandle<WorkerCounts>>,
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

    /// Block until every worker thread joins. Sums the per-worker
    /// counters into [`ShutdownReport`]. Synchronous: no async runtime
    /// required in the caller's context.
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

/// Bind every (worker × addr) socket and start accepting.
///
/// `setup` is a per-worker handler factory invoked inside the worker
/// thread (after CPU pinning + runtime build) so `H` may capture
/// `!Send` per-worker state.
//
// `spec` is taken by value at the public surface; callers expect a
// hand-off shape and asking for `&spec` everywhere buys nothing.
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

    let mut threads: Vec<thread::JoinHandle<WorkerCounts>> = Vec::with_capacity(n);
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
                let handler = (*setup)();
                run(
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

/// What a worker hands back when its OS thread joins.
#[derive(Debug, Default, Clone, Copy)]
struct WorkerCounts {
    accepted: u64,
    completed: u64,
}

struct Counts {
    accepted: Cell<u64>,
    completed: Cell<u64>,
}

// Each parameter is a distinct concern at this boundary; bundling them
// into a single `Config` struct just to silence clippy would be exactly
// the speculative abstraction the engineering charter forbids.
#[allow(clippy::too_many_arguments)]
fn run<H, Fut>(
    listeners: Vec<(SocketAddr, std::net::TcpListener)>,
    handler: H,
    flag: &Arc<AtomicBool>,
    uring_entries: u32,
    deadline: Duration,
    tcp_nodelay: bool,
    poll_interval: Duration,
    core: Option<core_affinity::CoreId>,
) -> WorkerCounts
where
    H: Fn(TcpStream, SocketAddr) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    if let Some(c) = core {
        let _ = core_affinity::set_for_current(c);
    }

    let mut rt = match monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(uring_entries)
        .enable_timer()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build monoio io_uring runtime");
            return WorkerCounts::default();
        }
    };

    let counts = Rc::new(Counts {
        accepted: Cell::new(0),
        completed: Cell::new(0),
    });
    let handler = Rc::new(handler);

    rt.block_on(async {
        let mut accept_tasks = Vec::with_capacity(listeners.len());
        for (addr, std_l) in listeners {
            let l = match TcpListener::from_std(std_l) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        %addr,
                        "monoio listener conversion failed inside worker; this listener is skipped",
                    );
                    continue;
                }
            };
            let h = Rc::clone(&handler);
            let c = Rc::clone(&counts);
            let f = Arc::clone(flag);
            accept_tasks.push(monoio::spawn(accept_loop(
                l,
                h,
                c,
                f,
                tcp_nodelay,
                poll_interval,
            )));
        }
        for t in accept_tasks {
            let () = t.await;
        }

        // Drain phase. Connection tasks were spawned via `monoio::spawn`
        // inside `accept_loop`; their completion is signalled by
        // `counts.completed`. If we returned now, monoio would drop the
        // runtime and abort them. Instead we poll until counts equalize
        // or the deadline elapses.
        let drain_start = std::time::Instant::now();
        loop {
            if counts.accepted.get() == counts.completed.get() {
                break;
            }
            if drain_start.elapsed() >= deadline {
                tracing::warn!(
                    accepted = counts.accepted.get(),
                    completed = counts.completed.get(),
                    "shutdown deadline elapsed; in-flight tasks aborted on runtime drop",
                );
                break;
            }
            monoio::time::sleep(poll_interval).await;
        }
    });

    WorkerCounts {
        accepted: counts.accepted.get(),
        completed: counts.completed.get(),
    }
}

async fn accept_loop<H, Fut>(
    listener: TcpListener,
    handler: Rc<H>,
    counts: Rc<Counts>,
    flag: Arc<AtomicBool>,
    tcp_nodelay: bool,
    poll_interval: Duration,
) where
    H: Fn(TcpStream, SocketAddr) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    loop {
        if flag.load(Ordering::Acquire) {
            break;
        }
        match monoio::time::timeout(poll_interval, listener.accept()).await {
            Ok(Ok((stream, peer))) => {
                counts.accepted.set(counts.accepted.get() + 1);
                if tcp_nodelay {
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::warn!(error = %e, "set_nodelay failed");
                    }
                }
                let h = Rc::clone(&handler);
                let c = Rc::clone(&counts);
                monoio::spawn(async move {
                    h(stream, peer).await;
                    c.completed.set(c.completed.get() + 1);
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
