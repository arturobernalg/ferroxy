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
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use bytes::Bytes;
use clap::Parser;
use conduit_io::{serve, ServeSpec, ShutdownReport};
use conduit_lifecycle::Dispatch;
use conduit_proto::{Response, StatusCode, VecBody};
use hyper::body::Incoming;

/// Live dispatch table — hot-swapped under SIGHUP. The hot path
/// loads a snapshot once per request and never takes a Mutex.
type DispatchSwap = Arc<ArcSwap<Dispatch>>;

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

    if cfg.server.listen_http.is_empty()
        && cfg.server.listen_https.is_empty()
        && cfg.server.listen_h3.is_empty()
    {
        tracing::error!(
            "no plain-HTTP, HTTPS, or H3 listeners; nothing to serve. \
             Add at least one entry to server.listen_http, server.listen_https, \
             or server.listen_h3.",
        );
        return ExitCode::from(78);
    }

    let spec = build_serve_spec(&cfg);
    let dispatches = build_dispatch_bundle(&cfg, spec.workers.get());
    let dispatch = Arc::clone(&dispatches.primary);
    let worker_dispatches = Arc::clone(&dispatches.http_workers);

    // Single shared metrics sink. Every per-request handler bumps
    // counters on it via `fetch_add(_, Relaxed)`; the admin server
    // reads it on every `/metrics` scrape. Charter rule: no Mutex.
    let metrics = Arc::new(conduit_control::MetricsHandle::new());

    // Spawn HTTPS listener if `[tls]` and `listen_https` are both
    // configured. The HTTPS listener runs on its own tokio runtime
    // thread so its TLS handshake cost does not interfere with the
    // plaintext data-plane runtime under load.
    let mut cert_resolver: Option<Arc<conduit_transport::MultiCertResolver>> = None;
    let https_thread = spawn_https_thread(&cfg, &dispatch, &metrics, &mut cert_resolver);

    // Reload thread can hot-swap both routes/upstreams (every
    // Dispatch in the bundle) and TLS certs (the resolver picked up
    // by spawn_https_thread). Routes-only reload still works when
    // no HTTPS is configured.
    let (reload_done, reload_thread) =
        start_reload(&cli.config, &dispatches, cert_resolver.clone());

    // Spawn HTTP/3 (QUIC) listener if `[tls]` and `listen_h3` are both
    // configured. Like HTTPS this runs on its own tokio runtime thread.
    let h3_thread = spawn_h3_thread(&cfg, &dispatch, &metrics);

    // Spawn the admin endpoint server on its own tokio runtime
    // thread. Sharing the data-plane runtime would couple admin
    // latency to data-plane scheduling pressure under load.
    let admin_addr = cfg.server.admin_listen;
    let admin_cancel = Arc::new(AtomicBool::new(false));
    let admin_thread = spawn_admin_thread(
        admin_addr,
        Arc::clone(&admin_cancel),
        Arc::clone(&metrics),
        upstreams_renderer(&dispatch),
    );

    let (health_cancel, health_thread) = start_health_probes(&cfg, &dispatch);

    let server = if cfg.server.listen_http.is_empty() {
        None
    } else {
        match start_http_server(spec, Arc::clone(&worker_dispatches), Arc::clone(&metrics)) {
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
    let h3_cancel_for_signal = h3_thread.as_ref().map(|(_, c)| Arc::clone(c));
    let admin_cancel_for_signal = Arc::clone(&admin_cancel);

    let (signal_received, signal_thread) = spawn_signal_thread(
        http_trigger,
        https_cancel_for_signal,
        h3_cancel_for_signal,
        admin_cancel_for_signal,
    );

    let report = match server {
        Some(s) => s.wait(),
        None => {
            // No HTTP plane; just block on https/admin via thread joins.
            ShutdownReport::default()
        }
    };

    join_background(&reload_done, reload_thread, &health_cancel, health_thread);

    join_aux_threads(
        &admin_cancel,
        admin_thread,
        https_thread,
        h3_thread,
        &signal_received,
        signal_thread,
    );

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
async fn proxy_one<B>(
    dispatch: &Dispatch,
    metrics: &conduit_control::MetricsHandle,
    req: http::Request<B>,
) -> Result<Response<ProxyBody>, Infallible>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    metrics.observe_request_start();
    let started = std::time::Instant::now();
    let result = dispatch.handle(req).await;
    let elapsed = started.elapsed();
    // Determine the status we'll surface so per-class histogram +
    // per-class counter agree. Recorded *before* the body
    // conversion / synthetic-error builders so the histogram
    // represents end-to-end routing + upstream wall time; per-stage
    // breakdowns are P11.5 scope.
    let status_for_metrics = match &result {
        Ok(resp) => resp.status().as_u16(),
        Err(conduit_lifecycle::DispatchError::NoRoute) => 404,
        Err(conduit_lifecycle::DispatchError::Upstream(e)) => {
            if matches!(e, conduit_upstream::ForwardError::BreakerOpen) {
                503
            } else {
                502
            }
        }
        Err(conduit_lifecycle::DispatchError::Timeout(_)) => 504,
        // UpstreamNotRegistered + any future non_exhaustive variant.
        Err(_) => 500,
    };
    metrics.observe_request_duration(elapsed, status_for_metrics);
    match result {
        Ok(resp) => {
            metrics.observe_status(resp.status().as_u16());
            let (parts, body) = resp.into_parts();
            // Wrap the upstream's body in our concrete enum instead
            // of allocating a Box<dyn Body>. Saves one heap
            // allocation per request on the success path.
            Ok(Response::from_parts(
                parts,
                ProxyBody::Upstream { inner: body },
            ))
        }
        Err(conduit_lifecycle::DispatchError::NoRoute) => {
            tracing::debug!("no route matched");
            metrics.observe_no_route();
            metrics.observe_status(404);
            Ok(error_response(
                StatusCode::NOT_FOUND,
                "conduit: no route matched\n",
            ))
        }
        Err(conduit_lifecycle::DispatchError::UpstreamNotRegistered { name }) => {
            tracing::error!(upstream = %name, "route references unknown upstream");
            metrics.observe_upstream_unknown();
            metrics.observe_status(500);
            Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conduit: route references unknown upstream\n",
            ))
        }
        Err(conduit_lifecycle::DispatchError::Upstream(e)) => {
            tracing::warn!(error = ?e, "upstream forward failed");
            metrics.observe_upstream_failed();
            // Distinguish breaker-open (fail-fast, expected during a
            // backend incident) from other forward failures so
            // operators can see the difference in dashboards.
            let (status, body) = if matches!(e, conduit_upstream::ForwardError::BreakerOpen) {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "conduit: upstream breaker open\n",
                )
            } else {
                (StatusCode::BAD_GATEWAY, "conduit: upstream unavailable\n")
            };
            metrics.observe_status(status.as_u16());
            Ok(error_response(status, body))
        }
        Err(conduit_lifecycle::DispatchError::Timeout(total)) => {
            tracing::warn!(?total, "request exceeded route total timeout");
            metrics.observe_upstream_failed();
            metrics.observe_status(504);
            Ok(error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "conduit: upstream timed out\n",
            ))
        }
        Err(other) => {
            // `DispatchError` is non_exhaustive so future variants
            // (e.g. P5.x's filter-rejection) compile here without
            // editing this match. They get a generic 500.
            tracing::error!(error = ?other, "dispatch error");
            metrics.observe_status(500);
            Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conduit: dispatch error\n",
            ))
        }
    }
}

fn error_response(status: StatusCode, body: &'static str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(ProxyBody::Synthetic {
            inner: VecBody::from_bytes(Bytes::from_static(body.as_bytes())),
        })
        .expect("build error response")
}

pin_project_lite::pin_project! {
    /// Body returned by [`proxy_one`]. Concrete enum (not `BoxBody`)
    /// so the per-request happy path doesn't pay for a
    /// `Box<dyn Body>` allocation. Two variants cover every shape
    /// conduit produces:
    ///
    /// - `Upstream` — bytes streamed back from the real backend,
    ///   wrapped in a per-frame read-timeout enforcer
    ///   (`conduit_upstream::TimedBody`).
    /// - `Synthetic` — error pages or admin replies built in-memory.
    ///
    /// `pin_project_lite` is required because `TimedBody` is `!Unpin`
    /// (its inner `tokio::time::Sleep` is `!Unpin`); the projection
    /// macro lets us forward pin access without unsafe.
    #[project = ProxyBodyProj]
    enum ProxyBody {
        Upstream {
            #[pin]
            inner: conduit_upstream::TimedBody<Incoming>,
        },
        Synthetic {
            inner: VecBody,
        },
    }
}

impl http_body::Body for ProxyBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        match self.project() {
            ProxyBodyProj::Upstream { inner } => {
                // TimedBody::Error is already Box<dyn Error+Send+Sync>.
                inner.poll_frame(cx)
            }
            ProxyBodyProj::Synthetic { inner } => std::pin::Pin::new(inner)
                .poll_frame(cx)
                .map(|opt| opt.map(|r| r.map_err(|never| match never {}))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ProxyBody::Upstream { inner } => inner.is_end_stream(),
            ProxyBody::Synthetic { inner } => inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            ProxyBody::Upstream { inner } => inner.size_hint(),
            ProxyBody::Synthetic { inner } => inner.size_hint(),
        }
    }
}

/// Bind the plaintext-HTTP listeners and start the conduit-io serve
/// loop. Each accepted TCP stream is handed to `conduit-h1::serve_connection`
/// with a per-request service that calls `Dispatch::handle`.
///
/// Per-worker pool sharding: `worker_dispatches` holds N independent
/// `Dispatch`es (one per worker — built from the same `Config` so
/// they're functionally identical, but each holds its own
/// `Upstream::client` and per-addr breakers). The `setup` closure
/// is called once per worker per listener; a `fetch_add` counter
/// dispenses dispatches in order so each worker gets its own pool
/// shard. SIGHUP reloads update every entry in the vec.
fn start_http_server(
    spec: ServeSpec,
    worker_dispatches: Arc<[DispatchSwap]>,
    metrics: Arc<conduit_control::MetricsHandle>,
) -> Result<conduit_io::Server, conduit_io::BindError> {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = worker_dispatches.len();
    let server = serve(spec, move || {
        // Pick this worker's Dispatch slot. `% n` keeps us in
        // bounds if `setup` is called more than N times (multi-
        // listener configs); single-listener configs see exactly
        // one call per worker.
        let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n;
        let dispatch = Arc::clone(&worker_dispatches[idx]);
        let metrics = Arc::clone(&metrics);
        move |stream: conduit_io::TcpStream, peer: std::net::SocketAddr| {
            let dispatch = Arc::clone(&dispatch);
            let metrics = Arc::clone(&metrics);
            async move {
                tracing::debug!(?peer, "accepted connection");
                let handler = move |req: http::Request<Incoming>| {
                    let dispatch = dispatch.load_full();
                    let metrics = Arc::clone(&metrics);
                    async move { proxy_one(&dispatch, &metrics, req).await }
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
    dispatch: &DispatchSwap,
    metrics: &Arc<conduit_control::MetricsHandle>,
    cert_resolver: &mut Option<Arc<conduit_transport::MultiCertResolver>>,
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
    let (server_cfg, resolver) = match conduit_transport::load_server_config_with_resolver(tls_cfg)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "TLS server config failed; HTTPS disabled");
            return None;
        }
    };
    // Stash the resolver for SIGHUP-driven hot-reload.
    *cert_resolver = Some(resolver);
    let acceptor = conduit_transport::build_acceptor(server_cfg);
    let dispatch_clone = Arc::clone(dispatch);
    let metrics_clone = Arc::clone(metrics);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);
    let addrs = cfg.server.listen_https.clone();
    let handle = std::thread::Builder::new()
        .name("conduit-https-rt".into())
        .spawn(move || run_https(addrs, acceptor, dispatch_clone, metrics_clone, cancel_clone))
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
    metrics: Arc<conduit_control::MetricsHandle>,
    upstreams: conduit_control::UpstreamsRenderer,
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
                if let Err(e) =
                    conduit_control::serve_admin(addr, cancel, metrics, Some(upstreams)).await
                {
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
    dispatch: DispatchSwap,
    metrics: Arc<conduit_control::MetricsHandle>,
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
            let metrics = Arc::clone(&metrics);
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
                    let metrics = Arc::clone(&metrics);
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
                            let dispatch = dispatch.load_full();
                            let metrics = Arc::clone(&metrics);
                            async move { proxy_one(&dispatch, &metrics, req).await }
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

/// SIGHUP-driven hot-reload setup: spawn the reload thread and
/// return its `done` flag (flipped on shutdown to break the wait)
/// together with the join handle. Re-reads the config from `path` on
/// each signal and atomically swaps the live Dispatch. In-flight
/// requests keep their old snapshot.
fn start_reload(
    path: &Path,
    bundle: &DispatchBundle,
    cert_resolver: Option<Arc<conduit_transport::MultiCertResolver>>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let done = Arc::new(AtomicBool::new(false));
    // Collect every swappable into one Vec so the reload thread
    // updates them all atomically (each store is independent; the
    // worst case during reload is a window where some workers see
    // the new config and others see the old, which is fine —
    // requests using the old keep their old snapshot to completion).
    let mut all: Vec<DispatchSwap> = Vec::with_capacity(1 + bundle.http_workers.len());
    all.push(Arc::clone(&bundle.primary));
    for d in bundle.http_workers.iter() {
        all.push(Arc::clone(d));
    }
    let handle = spawn_reload_thread(
        path.to_path_buf(),
        Arc::from(all),
        cert_resolver,
        Arc::clone(&done),
    );
    (done, handle)
}

/// Spawn a thread that listens for SIGHUP and atomically swaps every
/// live `Dispatch` with a fresh one built from the config file at
/// `path`. If a cert resolver is supplied, certs are also reloaded
/// from disk. Returns the join handle; `done` flipping true causes
/// the thread to exit on the next signal (or wake-up).
fn spawn_reload_thread(
    path: PathBuf,
    dispatches: Arc<[DispatchSwap]>,
    cert_resolver: Option<Arc<conduit_transport::MultiCertResolver>>,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("conduit-reload".into())
        .spawn(move || {
            install_reload_handler(&path, &dispatches, cert_resolver.as_deref(), &done);
        })
        .expect("spawn reload thread")
}

/// Block on SIGHUP, re-read the config, build a new `Dispatch` per
/// swappable, and atomically `store` each. Cert resolver, if
/// supplied, gets `reload()` called too — fresh PEM reads, atomic
/// table swap. Errors during reload are logged and the live
/// dispatchers + resolver are left untouched (charter rule: a bad
/// reload must not break a running proxy).
fn install_reload_handler(
    path: &Path,
    dispatches: &[DispatchSwap],
    cert_resolver: Option<&conduit_transport::MultiCertResolver>,
    done: &AtomicBool,
) {
    use signal_hook::consts::signal::SIGHUP;
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGHUP handler; hot-reload disabled");
            return;
        }
    };
    for _signal in signals.forever() {
        if done.load(Ordering::Acquire) {
            return;
        }
        tracing::info!(path = %path.display(), "received SIGHUP; reloading config");
        match conduit_config::load(path) {
            Ok(cfg) => {
                // Build one fresh Dispatch *per swappable*. Each
                // worker gets its own per-worker `Upstream::client`
                // — fresh hyper-util pool, fresh per-addr breakers.
                for swap in dispatches {
                    swap.store(Arc::new(Dispatch::from_config(&cfg)));
                }
                // Hot-reload TLS certs if HTTPS is configured. A bad
                // PEM file leaves the previous cert table in place
                // and just logs; HTTPS keeps serving uninterrupted.
                if let (Some(resolver), Some(tls_cfg)) = (cert_resolver, cfg.tls.as_ref()) {
                    if let Err(e) = resolver.reload(&tls_cfg.certs) {
                        tracing::error!(error = %e, "cert hot-reload failed; keeping previous certs");
                    } else {
                        tracing::info!(certs = tls_cfg.certs.len(), "TLS cert table reloaded",);
                    }
                }
                tracing::info!(
                    upstreams = cfg.upstreams.len(),
                    routes = cfg.routes.len(),
                    swappables = dispatches.len(),
                    "reload succeeded; new dispatches active",
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "reload failed; keeping previous config");
            }
        }
    }
}

/// Spawn the signal-handler thread. Returns the parent-done flag (so
/// the main thread can short-circuit the handler on a clean shutdown
/// that did not come from a signal) and the join handle.
fn spawn_signal_thread(
    http_trigger: Option<conduit_io::ShutdownTrigger>,
    https_cancel: Option<Arc<AtomicBool>>,
    h3_cancel: Option<Arc<AtomicBool>>,
    admin_cancel: Arc<AtomicBool>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let signal_received = Arc::new(AtomicBool::new(false));
    let signal_received_for_thread = Arc::clone(&signal_received);
    let handle = std::thread::Builder::new()
        .name("conduit-signal".into())
        .spawn(move || {
            install_signal_handler(&signal_received_for_thread, move || {
                if let Some(t) = http_trigger.as_ref() {
                    t.cancel();
                }
                if let Some(c) = https_cancel.as_ref() {
                    c.store(true, Ordering::Release);
                }
                if let Some(c) = h3_cancel.as_ref() {
                    c.store(true, Ordering::Release);
                }
                admin_cancel.store(true, Ordering::Release);
            });
        })
        .expect("spawn signal handler thread");
    (signal_received, handle)
}

/// Stop and join the long-lived background threads (reload + health
/// probes) that aren't tied to the data-plane runtime. Called once
/// the data plane has reported shutdown but before the aux threads
/// are joined.
fn join_background(
    reload_done: &Arc<AtomicBool>,
    reload_thread: std::thread::JoinHandle<()>,
    health_cancel: &Arc<AtomicBool>,
    health_thread: Option<std::thread::JoinHandle<()>>,
) {
    reload_done.store(true, Ordering::Release);
    let _ = reload_thread.join();
    health_cancel.store(true, Ordering::Release);
    if let Some(h) = health_thread {
        let _ = h.join();
    }
}

/// Stop and join the admin / https / h3 / signal-handler threads
/// after the data plane has reported shutdown. Each cancel flag is
/// flipped before its thread is joined so the loops exit promptly.
fn join_aux_threads(
    admin_cancel: &Arc<AtomicBool>,
    admin_thread: Option<std::thread::JoinHandle<()>>,
    https_thread: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>,
    h3_thread: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>,
    signal_received: &Arc<AtomicBool>,
    signal_thread: std::thread::JoinHandle<()>,
) {
    admin_cancel.store(true, Ordering::Release);
    if let Some(h) = admin_thread {
        let _ = h.join();
    }
    if let Some((h, cancel)) = https_thread {
        cancel.store(true, Ordering::Release);
        let _ = h.join();
    }
    if let Some((h, cancel)) = h3_thread {
        cancel.store(true, Ordering::Release);
        let _ = h.join();
    }
    signal_received.store(true, Ordering::Release);
    let _ = signal_thread.join();
}

/// Spawn the H3 listener thread if `[tls]` is configured and at
/// least one H3 listen address is set. Returns `None` if H3 is
/// disabled or the TLS config could not be loaded; the binary
/// continues without H3 in that case.
fn spawn_h3_thread(
    cfg: &conduit_config::Config,
    dispatch: &DispatchSwap,
    metrics: &Arc<conduit_control::MetricsHandle>,
) -> Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> {
    if cfg.server.listen_h3.is_empty() {
        return None;
    }
    let Some(tls_cfg) = cfg.tls.as_ref() else {
        tracing::error!(
            count = cfg.server.listen_h3.len(),
            "H3 listeners configured but no [tls] section; ignoring",
        );
        return None;
    };
    // Load a fresh ServerConfig and override its ALPN to advertise
    // only `h3` — QUIC negotiates ALPN as part of the TLS handshake
    // and rejects connections whose offered protocols don't intersect.
    let mut rustls_cfg = match conduit_transport::load_server_config(tls_cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "TLS server config failed; H3 disabled");
            return None;
        }
    };
    rustls_cfg.alpn_protocols = vec![b"h3".to_vec()];

    let quic_cfg = match conduit_h3::quic_server_config(rustls_cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "QUIC server config failed; H3 disabled");
            return None;
        }
    };

    let dispatch_clone = Arc::clone(dispatch);
    let metrics_clone = Arc::clone(metrics);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);
    let addrs = cfg.server.listen_h3.clone();
    let handle = std::thread::Builder::new()
        .name("conduit-h3-rt".into())
        .spawn(move || run_h3(addrs, quic_cfg, dispatch_clone, metrics_clone, cancel_clone))
        .expect("spawn h3 thread");
    Some((handle, cancel))
}

/// Drive the H3 listeners on a dedicated thread. Builds a
/// multi-thread tokio runtime, opens a `quinn::Endpoint` per
/// `listen_h3` address, and accepts QUIC connections in a loop until
/// `cancel` is set. Each connection is handed to
/// `conduit-h3::serve_connection`.
fn run_h3(
    addrs: Vec<std::net::SocketAddr>,
    quic_cfg: quinn::ServerConfig,
    dispatch: DispatchSwap,
    metrics: Arc<conduit_control::MetricsHandle>,
    cancel: Arc<AtomicBool>,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("conduit-h3")
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build h3 runtime");
            return;
        }
    };
    rt.block_on(async move {
        let mut endpoints = Vec::with_capacity(addrs.len());
        for addr in &addrs {
            match quinn::Endpoint::server(quic_cfg.clone(), *addr) {
                Ok(ep) => {
                    let local = ep.local_addr().unwrap_or(*addr);
                    tracing::info!(addr = %local, "listening (h3/quic)");
                    endpoints.push(ep);
                }
                Err(e) => {
                    tracing::error!(addr = %addr, error = %e, "h3 bind failed");
                    return;
                }
            }
        }

        let mut tasks = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let dispatch = Arc::clone(&dispatch);
            let metrics = Arc::clone(&metrics);
            let cancel = Arc::clone(&cancel);
            tasks.push(tokio::spawn(async move {
                while !cancel.load(Ordering::Acquire) {
                    let timed = tokio::time::timeout(
                        std::time::Duration::from_millis(250),
                        endpoint.accept(),
                    )
                    .await;
                    let connecting = match timed {
                        Ok(Some(c)) => c,
                        Ok(None) => break,  // endpoint closed
                        Err(_) => continue, // timeout → recheck cancel
                    };
                    let dispatch = Arc::clone(&dispatch);
                    let metrics = Arc::clone(&metrics);
                    tokio::spawn(async move {
                        let conn = match connecting.await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::debug!(error = %e, "quic handshake failed");
                                return;
                            }
                        };
                        let peer = conn.remote_address();
                        let handler = move |req: http::Request<Bytes>| {
                            let dispatch = dispatch.load_full();
                            let metrics = Arc::clone(&metrics);
                            async move {
                                let (parts, body) = req.into_parts();
                                let req = http::Request::from_parts(
                                    parts,
                                    http_body_util::Full::new(body),
                                );
                                proxy_one(&dispatch, &metrics, req).await
                            }
                        };
                        if let Err(e) = conduit_h3::serve_connection(conn, handler).await {
                            tracing::warn!(?peer, error = %e, "h3 connection ended with error");
                        }
                    });
                }
                endpoint.close(0u32.into(), b"shutting down");
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });
}

/// Build the I/O serve spec from the config.
fn build_serve_spec(cfg: &conduit_config::Config) -> ServeSpec {
    let workers = resolve_workers(cfg.server.workers);
    let mut spec = ServeSpec::new(cfg.server.listen_http.clone(), workers);
    spec.cpu_affinity = cfg.server.cpu_affinity;
    spec
}

/// Build the lifecycle dispatcher from the config, wrapped in an
/// `ArcSwap` so SIGHUP can replace it atomically without taking a
/// lock on the hot path. Each request loads a snapshot `Arc` once.
fn build_dispatch_swap(cfg: &conduit_config::Config) -> DispatchSwap {
    Arc::new(ArcSwap::new(Arc::new(Dispatch::from_config(cfg))))
}

/// Bundle of `Dispatch`es shared across the binary's planes. The
/// primary is used by HTTPS / H3 / admin / reload / health (each on
/// their own dedicated thread); the HTTP plain plane gets N
/// per-worker dispatches so each worker thread has its own
/// `Upstream::client` (and therefore its own `hyper-util`
/// `Mutex<Pool>` instance, eliminating cross-worker pool contention).
///
/// Each dispatch is functionally identical (built from the same
/// `Config`) but holds its own per-upstream state: the pool, the
/// per-addr breakers, and the round-robin counter.
struct DispatchBundle {
    /// Used by every non-HTTP plane. One swappable.
    primary: DispatchSwap,
    /// One swappable per HTTP plain plane worker. The serve loop
    /// hands each worker its own slot via a `fetch_add` counter.
    http_workers: Arc<[DispatchSwap]>,
}

fn build_dispatch_bundle(cfg: &conduit_config::Config, n_http_workers: usize) -> DispatchBundle {
    let primary = build_dispatch_swap(cfg);
    let http_workers: Vec<DispatchSwap> = (0..n_http_workers)
        .map(|_| build_dispatch_swap(cfg))
        .collect();
    DispatchBundle {
        primary,
        http_workers: http_workers.into(),
    }
}

/// Active health checks. Spawn a dedicated thread + tokio runtime so
/// probe latency does not bleed into the data plane (same pattern as
/// the admin server). Probes update the same per-addr breakers the
/// request hot path consults — a backend that fails probes goes Open
/// and is rotated out without waiting for real traffic to discover
/// it. Returns the cancel flag (flipped on shutdown) and the optional
/// join handle (`None` if no upstream has health checks configured).
fn start_health_probes(
    cfg: &conduit_config::Config,
    dispatch: &DispatchSwap,
) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = spawn_health_thread(cfg, dispatch, &cancel);
    (cancel, handle)
}

/// Spawn a dedicated thread + tokio runtime that drives active
/// health probes for every upstream with `[upstream.health_check]`
/// configured. Returns `None` if no upstream has health checks
/// configured (no thread is spawned in that case).
fn spawn_health_thread(
    cfg: &conduit_config::Config,
    dispatch: &DispatchSwap,
    cancel: &Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    // Pair each (upstream-name, probe-config) with the live Upstream
    // we'd build a Probe for. Doing this on the calling thread (sync,
    // before the thread spawn) keeps the prober task self-contained
    // — it doesn't need to reach into the ArcSwap on every iteration.
    let snapshot = dispatch.load_full();
    let probes: Vec<conduit_upstream::Probe> = cfg
        .upstreams
        .iter()
        .filter_map(|u| {
            let hc = u.health_check.as_ref()?;
            let upstream = snapshot.upstreams().get(&u.name)?;
            // Probe over HTTPS when the upstream itself is configured
            // for TLS — same CA bundle, same mTLS client cert, same
            // verify policy. This way a TLS-only backend that is
            // reachable from the data plane is also reachable from
            // the prober.
            let probe_tls = u
                .tls
                .as_ref()
                .map(|t| conduit_upstream::UpstreamTlsOptions {
                    ca: t.ca.clone(),
                    client_cert: t.client_cert.clone(),
                    client_key: t.client_key.clone(),
                    verify: t.verify,
                });
            match conduit_upstream::Probe::new(
                upstream.addrs_arc(),
                upstream.breakers_arc(),
                conduit_upstream::ProbeConfig {
                    path: hc.path.clone(),
                    interval: hc.interval.into(),
                    timeout: hc.timeout.into(),
                    unhealthy_threshold: hc.unhealthy_threshold,
                    healthy_threshold: hc.healthy_threshold,
                    tls: probe_tls,
                },
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::error!(
                        upstream = %u.name,
                        error = %e,
                        "could not build active probe (TLS load failure); skipping",
                    );
                    None
                }
            }
        })
        .collect();
    if probes.is_empty() {
        return None;
    }
    let cancel_clone = Arc::clone(cancel);
    Some(
        std::thread::Builder::new()
            .name("conduit-health-rt".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build health-check runtime");
                        return;
                    }
                };
                rt.block_on(async move {
                    let mut tasks = Vec::with_capacity(probes.len());
                    for probe in probes {
                        let cancel = Arc::clone(&cancel_clone);
                        tasks.push(tokio::spawn(async move { probe.run(cancel).await }));
                    }
                    for t in tasks {
                        let _ = t.await;
                    }
                });
            })
            .expect("spawn health thread"),
    )
}

/// Build a renderer thunk that walks the live `Dispatch` (loaded from
/// the shared `ArcSwap` on each scrape, so SIGHUP reloads are
/// reflected immediately) and produces the `/upstreams` body.
fn upstreams_renderer(dispatch: &DispatchSwap) -> conduit_control::UpstreamsRenderer {
    let dispatch = Arc::clone(dispatch);
    Arc::new(move || render_upstreams(&dispatch.load_full()))
}

/// Render the `/upstreams` admin response body. Plain text, one line
/// per upstream, e.g.:
///
/// ```text
/// upstream api  addrs=["10.0.0.1:8080","10.0.0.2:8080"]  breaker=closed
/// upstream static  addrs=["10.0.0.10:8080"]              breaker=open
/// ```
fn render_upstreams(d: &Dispatch) -> String {
    let mut entries: Vec<(&str, &conduit_upstream::Upstream)> = d.upstreams().iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    let mut out = String::with_capacity(entries.len() * 80);
    for (name, u) in entries {
        out.push_str("upstream ");
        out.push_str(name);
        out.push_str(" addrs=[");
        for (i, a) in u.addrs().iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(&a.to_string());
            out.push('"');
        }
        out.push_str("] breaker=");
        out.push_str(match u.breaker_state() {
            conduit_upstream::BreakerState::Closed => "closed",
            conduit_upstream::BreakerState::Open => "open",
            conduit_upstream::BreakerState::HalfOpen => "half_open",
        });
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("(no upstreams)\n");
    }
    out
}

fn print_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("  error: {err}");
    let mut src = err.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}
