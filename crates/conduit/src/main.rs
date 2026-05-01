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

    if !cfg.server.listen_h3.is_empty() {
        tracing::warn!(
            count = cfg.server.listen_h3.len(),
            "H3 listeners ignored: HTTP/3 ships in phase 9",
        );
    }
    if cfg.server.listen_http.is_empty() && cfg.server.listen_https.is_empty() {
        tracing::error!(
            "no plain-HTTP or HTTPS listeners; nothing to serve. \
             Add at least one entry to server.listen_http or server.listen_https.",
        );
        return ExitCode::from(78);
    }

    let workers = resolve_workers(cfg.server.workers);
    let mut spec = ServeSpec::new(cfg.server.listen_http.clone(), workers);
    spec.cpu_affinity = cfg.server.cpu_affinity;

    // Build the lifecycle dispatcher from the config. Arc-wrapped so
    // the per-connection handler closures share one read-only copy.
    let dispatch = Arc::new(Dispatch::from_config(&cfg));

    // Spawn HTTPS listener if `[tls]` and `listen_https` are both
    // configured. The HTTPS listener runs on its own tokio runtime
    // thread so its TLS handshake cost does not interfere with the
    // plaintext data-plane runtime under load.
    let https_thread = spawn_https_thread(&cfg, &dispatch);

    // Spawn the admin endpoint server on its own tokio runtime
    // thread. Sharing the data-plane runtime would couple admin
    // latency to data-plane scheduling pressure under load.
    let admin_addr = cfg.server.admin_listen;
    let admin_cancel = Arc::new(AtomicBool::new(false));
    let admin_thread = spawn_admin_thread(admin_addr, Arc::clone(&admin_cancel));

    let server = if cfg.server.listen_http.is_empty() {
        None
    } else {
        match start_http_server(spec, Arc::clone(&dispatch)) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(error = %e, "bind failed");
                print_chain(&e);
                return ExitCode::from(1);
            }
        }
    };

    let http_trigger = server.as_ref().map(conduit_io::Server::shutdown_trigger);
    let https_cancel_for_signal = https_thread.as_ref().map(|(_, c)| Arc::clone(c));
    let admin_cancel_for_signal = Arc::clone(&admin_cancel);

    // Signal handler thread: first SIGTERM/SIGINT triggers graceful
    // shutdown across the http data plane, the https data plane, and
    // the admin server.
    let signal_received = Arc::new(AtomicBool::new(false));
    let signal_received_for_thread = Arc::clone(&signal_received);
    let signal_thread = std::thread::Builder::new()
        .name("conduit-signal".into())
        .spawn(move || {
            install_signal_handler(&signal_received_for_thread, move || {
                if let Some(t) = http_trigger.as_ref() {
                    t.cancel();
                }
                if let Some(c) = https_cancel_for_signal.as_ref() {
                    c.store(true, Ordering::Release);
                }
                admin_cancel_for_signal.store(true, Ordering::Release);
            });
        })
        .expect("spawn signal handler thread");

    let report = match server {
        Some(s) => s.wait(),
        None => {
            // No HTTP plane; just block on https/admin via thread joins.
            ShutdownReport::default()
        }
    };

    // Stop admin + https.
    admin_cancel.store(true, Ordering::Release);
    if let Some(h) = admin_thread {
        let _ = h.join();
    }
    if let Some((h, cancel)) = https_thread {
        cancel.store(true, Ordering::Release);
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

/// Bind the plaintext-HTTP listeners and start the conduit-io serve
/// loop. Each accepted TCP stream is handed to `conduit-h1::serve_connection`
/// with a per-request service that calls `Dispatch::handle`.
fn start_http_server(
    spec: ServeSpec,
    dispatch: Arc<Dispatch>,
) -> Result<conduit_io::Server, conduit_io::BindError> {
    let server = serve(spec, move || {
        let dispatch = Arc::clone(&dispatch);
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
    })?;
    tracing::info!(addrs = ?server.local_addrs(), "listening (http)");
    Ok(server)
}

/// Spawn the HTTPS listener thread if `[tls]` is configured and at
/// least one HTTPS listen address is set. Returns `None` if HTTPS is
/// disabled or the TLS config could not be loaded; the binary
/// continues with HTTP-only in that case.
fn spawn_https_thread(
    cfg: &conduit_config::Config,
    dispatch: &Arc<Dispatch>,
) -> Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> {
    if cfg.server.listen_https.is_empty() {
        return None;
    }
    let Some(tls_cfg) = cfg.tls.as_ref() else {
        tracing::error!(
            count = cfg.server.listen_https.len(),
            "HTTPS listeners configured but no [tls] section; ignoring",
        );
        return None;
    };
    let server_cfg = match conduit_transport::load_server_config(tls_cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "TLS server config failed; HTTPS disabled");
            return None;
        }
    };
    let acceptor = conduit_transport::build_acceptor(server_cfg);
    let dispatch_clone = Arc::clone(dispatch);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);
    let addrs = cfg.server.listen_https.clone();
    let handle = std::thread::Builder::new()
        .name("conduit-https-rt".into())
        .spawn(move || run_https(addrs, acceptor, dispatch_clone, cancel_clone))
        .expect("spawn https thread");
    Some((handle, cancel))
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

/// Wait for SIGTERM or SIGINT, then run the supplied shutdown closure.
/// `parent_done` short-circuits the wait so the parent thread can join
/// the signal handler after a clean shutdown that did not come from a
/// signal (e.g. an internal serve error).
fn install_signal_handler(parent_done: &AtomicBool, on_signal: impl FnOnce()) {
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
        on_signal();
    }
}

/// Drive the HTTPS listeners on a dedicated thread. Builds a
/// multi-thread tokio runtime, binds each `listen_https` address, and
/// accepts in a loop until `cancel` is set. Each accepted TCP stream
/// is handed to `accept_tls` and then to `conduit-h1::serve_connection`
/// — same per-request path as the plaintext listener, just with a TLS
/// wrapper at the bottom of the stack.
fn run_https(
    addrs: Vec<std::net::SocketAddr>,
    acceptor: conduit_transport::TlsAcceptor,
    dispatch: Arc<Dispatch>,
    cancel: Arc<AtomicBool>,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("conduit-https")
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build https runtime");
            return;
        }
    };
    rt.block_on(async move {
        let mut listeners = Vec::with_capacity(addrs.len());
        for addr in &addrs {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => {
                    let local = l.local_addr().unwrap_or(*addr);
                    tracing::info!(addr = %local, "listening (https)");
                    listeners.push(l);
                }
                Err(e) => {
                    tracing::error!(addr = %addr, error = %e, "https bind failed");
                    return;
                }
            }
        }

        // One accept loop per listener. Each loop polls `cancel` between
        // accepts via a short timeout so SIGTERM unblocks shutdown
        // without waiting for the next connection.
        let mut tasks = Vec::with_capacity(listeners.len());
        for listener in listeners {
            let acceptor = acceptor.clone();
            let dispatch = Arc::clone(&dispatch);
            let cancel = Arc::clone(&cancel);
            tasks.push(tokio::spawn(async move {
                while !cancel.load(Ordering::Acquire) {
                    let accept = listener.accept();
                    let timed =
                        tokio::time::timeout(std::time::Duration::from_millis(250), accept).await;
                    let (stream, peer) = match timed {
                        Ok(Ok(p)) => p,
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "https accept failed");
                            continue;
                        }
                        Err(_) => continue, // timeout → recheck cancel
                    };
                    let acceptor = acceptor.clone();
                    let dispatch = Arc::clone(&dispatch);
                    tokio::spawn(async move {
                        let tls = match conduit_transport::accept_tls(&acceptor, stream).await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::debug!(?peer, error = %e, "tls handshake failed");
                                return;
                            }
                        };
                        // ALPN dispatch: rustls writes the negotiated
                        // protocol into the connection state. `h2` →
                        // conduit-h2, anything else (incl. http/1.1
                        // and absent ALPN) → conduit-h1.
                        let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                        let handler = move |req: http::Request<Incoming>| {
                            let dispatch = Arc::clone(&dispatch);
                            async move { proxy_one(&dispatch, req).await }
                        };
                        match alpn.as_deref() {
                            Some(b"h2") => {
                                if let Err(e) = conduit_h2::serve_connection(tls, handler).await {
                                    tracing::warn!(
                                        ?peer,
                                        error = %e,
                                        "https/h2 connection ended with error",
                                    );
                                }
                            }
                            _ => {
                                if let Err(e) = conduit_h1::serve_connection(tls, handler).await {
                                    tracing::warn!(
                                        ?peer,
                                        error = %e,
                                        "https/h1 connection ended with error",
                                    );
                                }
                            }
                        }
                    });
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });
}

fn print_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("  error: {err}");
    let mut src = err.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}
