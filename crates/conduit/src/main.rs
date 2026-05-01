//! conduit — reverse proxy binary.
//!
//! Loads + validates configuration, builds the lifecycle dispatch
//! table, binds HTTP/1.1 listeners via `conduit-io`, and serves real
//! HTTP traffic by handing each connection to `conduit-h1`'s
//! `serve_connection` with a service that calls `Dispatch::handle`.
//!
//! Runtime backend: tokio (`runtime-tokio` cargo feature, on by
//! default in this binary). The monoio backend in `conduit-io` is
//! production-targeted but waits on a monoio↔tokio bridge before the
//! binary can route real HTTP through it; the bridge is the next
//! deferred follow-up. TLS (P6), HTTP/2 (P7), and HTTP/3 (P9)
//! similarly arrive in their own phases.

#![deny(missing_docs)]

use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use clap::Parser;
use conduit_io::{serve, ServeSpec, ShutdownReport};
use conduit_lifecycle::Dispatch;
use conduit_proto::{boxed, Response, StatusCode, VecBody};
use http_body_util::BodyExt;
use hyper::body::Incoming;

mod log;

#[derive(Debug, Parser)]
#[command(name = "conduit", version, about = "Reverse proxy")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short = 'c', long, value_name = "PATH")]
    config: PathBuf,

    /// Validate the configuration and exit. Does not start the runtime.
    #[arg(long)]
    check: bool,

    /// Emit logs as JSON instead of the default text format.
    #[arg(long)]
    log_json: bool,
}

/// Exit codes are stable for use by init systems and CI:
///
/// - `0` — clean shutdown after a SIGTERM/SIGINT, or `--check` succeeded.
/// - `1` — runtime build failure or unexpected serve error.
/// - `2` — config could not be loaded or failed validation.
/// - `78` — config is structurally valid but has nothing this build can
///   serve (only HTTPS / H3 listeners while we are still in phase 1).
fn main() -> ExitCode {
    let cli = Cli::parse();
    log::init(cli.log_json);

    let cfg = match conduit_config::load(&cli.config) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("conduit: invalid config at {}", cli.config.display());
            print_chain(&err);
            return ExitCode::from(2);
        }
    };

    tracing::info!(
        upstreams = cfg.upstreams.len(),
        routes = cfg.routes.len(),
        listen_http = cfg.server.listen_http.len(),
        listen_https = cfg.server.listen_https.len(),
        listen_h3 = cfg.server.listen_h3.len(),
        "config valid",
    );

    if cli.check {
        return ExitCode::SUCCESS;
    }

    if !cfg.server.listen_https.is_empty() {
        tracing::warn!(
            count = cfg.server.listen_https.len(),
            "HTTPS listeners ignored: TLS termination ships in phase 6",
        );
    }
    if !cfg.server.listen_h3.is_empty() {
        tracing::warn!(
            count = cfg.server.listen_h3.len(),
            "H3 listeners ignored: HTTP/3 ships in phase 9",
        );
    }
    if cfg.server.listen_http.is_empty() {
        tracing::error!(
            "no plain-HTTP listeners; phase 1 has nothing to serve. \
             Add at least one entry to server.listen_http or wait for phase 6 (HTTPS).",
        );
        return ExitCode::from(78);
    }

    let workers = resolve_workers(cfg.server.workers);
    let mut spec = ServeSpec::new(cfg.server.listen_http.clone(), workers);
    spec.cpu_affinity = cfg.server.cpu_affinity;

    // Build the lifecycle dispatcher from the config. Arc-wrapped so
    // the per-connection handler closures share one read-only copy.
    let dispatch = Arc::new(Dispatch::from_config(&cfg));

    // Spawn the admin endpoint server on its own tokio runtime
    // thread. Sharing the data-plane runtime would couple admin
    // latency to data-plane scheduling pressure under load.
    let admin_addr = cfg.server.admin_listen;
    let admin_cancel = Arc::new(AtomicBool::new(false));
    let admin_thread = spawn_admin_thread(admin_addr, Arc::clone(&admin_cancel));

    let dispatch_for_setup = Arc::clone(&dispatch);
    let server = match serve(spec, move || {
        let dispatch = Arc::clone(&dispatch_for_setup);
        // The setup closure runs once per accept loop and produces the
        // connection-level handler. The handler hands the stream to
        // conduit-h1's serve_connection with a per-request service
        // that calls Dispatch::handle.
        move |stream: conduit_io::TcpStream, peer: std::net::SocketAddr| {
            let dispatch = Arc::clone(&dispatch);
            async move {
                tracing::debug!(?peer, "accepted connection");
                let handler = move |req: http::Request<Incoming>| {
                    let dispatch = Arc::clone(&dispatch);
                    async move { proxy_one(&dispatch, req).await }
                };
                if let Err(e) = conduit_h1::serve_connection(stream, handler).await {
                    tracing::warn!(?peer, error = %e, "connection ended with error");
                }
            }
        }
    }) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "bind failed");
            print_chain(&e);
            return ExitCode::from(1);
        }
    };
    tracing::info!(addrs = ?server.local_addrs(), "listening");

    let trigger = server.shutdown_trigger();

    // Signal handler thread: first SIGTERM/SIGINT triggers graceful
    // shutdown. Subsequent signals fall through to the OS default
    // disposition (process termination), which is the right escalation
    // for an unresponsive shutdown.
    let signal_received = Arc::new(AtomicBool::new(false));
    let signal_received_for_thread = Arc::clone(&signal_received);
    let signal_thread = std::thread::Builder::new()
        .name("conduit-signal".into())
        .spawn(move || install_signal_handler(&trigger, &signal_received_for_thread))
        .expect("spawn signal handler thread");

    let report: ShutdownReport = server.wait();

    // Stop the admin server too.
    admin_cancel.store(true, Ordering::Release);
    if let Some(h) = admin_thread {
        let _ = h.join();
    }

    signal_received.store(true, Ordering::Release);
    let _ = signal_thread.join();

    tracing::info!(
        accepted = report.accepted,
        completed = report.completed,
        elapsed_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        "shutdown complete",
    );
    if report.accepted == report.completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// One proxied request: route via `Dispatch`, forward to the named
/// upstream, return the response. On routing or upstream error,
/// build a synthetic 502 / 503 response so the client always gets a
/// well-formed reply.
async fn proxy_one(
    dispatch: &Dispatch,
    req: http::Request<Incoming>,
) -> Result<Response<conduit_proto::BoxBody>, Infallible> {
    match dispatch.handle(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            // Erase hyper::body::Incoming → BoxBody so the binary
            // returns one concrete body type regardless of which
            // upstream produced the bytes. Cold path; one box per
            // request — the per-request hot-path no-allocation rule
            // is met inside conduit-upstream's request loop.
            let body: conduit_proto::BoxBody = BodyExt::boxed(BodyExt::map_err(body, |e| {
                let b: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
                b
            }));
            Ok(Response::from_parts(parts, body))
        }
        Err(conduit_lifecycle::DispatchError::NoRoute) => {
            tracing::debug!("no route matched");
            Ok(error_response(
                StatusCode::NOT_FOUND,
                "conduit: no route matched\n",
            ))
        }
        Err(conduit_lifecycle::DispatchError::UpstreamNotRegistered { name }) => {
            tracing::error!(upstream = %name, "route references unknown upstream");
            Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conduit: route references unknown upstream\n",
            ))
        }
        Err(conduit_lifecycle::DispatchError::Upstream(e)) => {
            tracing::warn!(error = ?e, "upstream forward failed");
            Ok(error_response(
                StatusCode::BAD_GATEWAY,
                "conduit: upstream unavailable\n",
            ))
        }
        Err(other) => {
            // `DispatchError` is non_exhaustive so future variants
            // (e.g. P5.x's filter-rejection) compile here without
            // editing this match. They get a generic 500.
            tracing::error!(error = ?other, "dispatch error");
            Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conduit: dispatch error\n",
            ))
        }
    }
}

fn error_response(status: StatusCode, body: &'static str) -> Response<conduit_proto::BoxBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(boxed(VecBody::from_bytes(Bytes::from_static(
            body.as_bytes(),
        ))))
        .expect("build error response")
}

/// Spawn a dedicated OS thread running a single-threaded tokio
/// runtime that hosts the admin HTTP/1.1 server. Decoupled from the
/// data-plane runtime so admin latency does not bleed into the proxy
/// hot path under load. Returns `None` if the thread could not be
/// spawned (rare; the binary still starts and continues without
/// admin endpoints, with a warning).
fn spawn_admin_thread(
    addr: std::net::SocketAddr,
    cancel: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("conduit-admin-rt".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "failed to build admin runtime");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = conduit_control::serve_admin(addr, cancel).await {
                    tracing::error!(error = %e, "admin server failed");
                }
            });
        })
        .map_err(|e| {
            tracing::warn!(
                error = %e,
                "could not spawn admin thread; continuing without admin endpoints"
            );
            e
        })
        .ok()
}

fn resolve_workers(w: conduit_config::Workers) -> NonZeroUsize {
    match w {
        conduit_config::Workers::Auto => std::thread::available_parallelism()
            .unwrap_or_else(|_| NonZeroUsize::new(1).expect("1 is non-zero")),
        conduit_config::Workers::Count(n) => n,
    }
}

/// Wait for SIGTERM or SIGINT, then trigger graceful shutdown.
fn install_signal_handler(trigger: &conduit_io::ShutdownTrigger, parent_done: &AtomicBool) {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGTERM, SIGINT]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM/SIGINT handlers");
            return;
        }
    };
    if let Some(signal) = signals.forever().next() {
        if parent_done.load(Ordering::Acquire) {
            return;
        }
        match signal {
            SIGTERM => tracing::info!("received SIGTERM; initiating graceful shutdown"),
            SIGINT => tracing::info!("received SIGINT; initiating graceful shutdown"),
            other => {
                tracing::info!(
                    signal = other,
                    "received signal; initiating graceful shutdown"
                );
            }
        }
        trigger.cancel();
    }
}

fn print_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("  error: {err}");
    let mut src = err.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}
