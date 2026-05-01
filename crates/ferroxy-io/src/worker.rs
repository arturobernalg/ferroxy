//! One worker = one OS thread = one monoio current-thread runtime.
//!
//! Per worker, an accept loop runs for every listen address (each backed
//! by its own `SO_REUSEPORT`-bound listener). Connections are spawned as
//! monoio tasks on the same runtime — never crossing thread boundaries.
//!
//! Shutdown is observed by polling a shared `Arc<AtomicBool>` at
//! `poll_interval`. After cancellation, accept loops exit; the worker
//! main then drains in-flight connection tasks until either
//! `accepted == completed` or `shutdown_deadline` elapses, at which point
//! the runtime drops and remaining tasks abort.
//!
//! All per-worker state is local: handler counts live in `Rc<Cell<u64>>`
//! to keep the hot path off cross-thread atomics. Cross-thread totals are
//! returned from the OS thread's join handle.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use monoio::net::{TcpListener, TcpStream};

/// What a worker hands back when its OS thread joins.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WorkerCounts {
    pub accepted: u64,
    pub completed: u64,
}

struct Counts {
    accepted: Cell<u64>,
    completed: Cell<u64>,
}

// Each parameter is a distinct concern at this boundary; bundling them
// into a single `Config` struct just to silence clippy would be exactly
// the speculative abstraction the engineering charter forbids.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run<H, Fut>(
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

        // Drain phase. Connection tasks were spawned via monoio::spawn
        // inside accept_loop; their completion is signalled by
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
    // Cooperative shutdown via `timeout(poll_interval, accept)`. On
    // timeout we check the flag and either continue or break. This is
    // simpler than `select!` and does not depend on macro/biased
    // semantics that vary between async runtimes.
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
                // Most accept errors are transient (EMFILE, ECONNRESET,
                // ECONNABORTED). Log and continue.
                tracing::warn!(error = %e, "accept error");
            }
            Err(_elapsed) => {
                // Timed out — falling through to flag-check on next iter.
            }
        }
    }
}
