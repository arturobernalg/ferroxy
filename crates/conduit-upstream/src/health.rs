//! Active health checks.
//!
//! For each backend address in an upstream we periodically issue a
//! `GET <path>` over plaintext HTTP/1.1 and feed the outcome to the
//! addr's breaker. A run of `unhealthy_threshold` failures pushes
//! the breaker into Open without waiting for real traffic to discover
//! the failure; a run of `healthy_threshold` successes resets it.
//!
//! Probes run on whatever tokio runtime the caller drives via
//! [`Probe::run`]. The binary spawns a dedicated thread + runtime so
//! probe latency spikes do not bleed into the data plane (same
//! pattern as the admin server).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::Breaker;

/// Active-probe configuration for one upstream.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Path probed via GET. Should return 2xx when healthy.
    pub path: String,
    /// Probe interval.
    pub interval: Duration,
    /// Per-probe timeout.
    pub timeout: Duration,
    /// Consecutive failures that flip a backend to Open. The
    /// per-addr breaker is the source of truth, so this controls
    /// how aggressive the prober is *relative to* the breaker.
    pub unhealthy_threshold: u32,
    /// Consecutive successes that flip a backend back to Closed.
    /// One success is enough today; the breaker re-arms on a single
    /// half-open success. Surface area for stricter policy when we
    /// add it.
    pub healthy_threshold: u32,
    /// TLS options for HTTPS probes. `None` → plain HTTP probes
    /// (the default; matches a backend that speaks plain HTTP).
    /// `Some(_)` → HTTPS probes using the same TLS knobs the
    /// production upstream uses (custom CA, mTLS, verify toggle).
    /// The probe shares those knobs so a TLS-only backend that is
    /// reachable from the data plane is also reachable from the
    /// prober.
    pub tls: Option<crate::UpstreamTlsOptions>,
}

/// Probe Client type. Same connector shape as `crate::Connector` so
/// HTTPS probes share the upstream's TLS plumbing (custom CA, mTLS,
/// verify toggle) while HTTP probes still work without TLS.
type ProbeClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Empty<Bytes>,
>;

/// One configured active probe for one upstream pool.
///
/// Construct one per `(addrs, ProbeConfig)` pair, then call
/// [`Probe::run`] on a tokio runtime; `run` returns a `Future` that
/// drives every addr's loop concurrently and resolves only when
/// `cancel` is dropped.
pub struct Probe {
    addrs: Arc<[SocketAddr]>,
    breakers: Arc<[Breaker]>,
    cfg: ProbeConfig,
    client: ProbeClient,
    /// The scheme the probe uses for its URL (`http` or `https`).
    /// Captured at construction so each probe iteration doesn't
    /// re-decide; flips with `cfg.tls.is_some()`.
    scheme: &'static str,
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Probe")
            .field("addrs", &self.addrs)
            .field("interval", &self.cfg.interval)
            .field("path", &self.cfg.path)
            .finish_non_exhaustive()
    }
}

impl Probe {
    /// Build a probe sharing the supplied `addrs` and `breakers`
    /// arrays with an `Upstream` (so probe outcomes update the same
    /// breakers the request hot path consults).
    ///
    /// Returns `Err` only when `cfg.tls` is `Some` and one of the
    /// configured PEM files (custom CA, client cert, client key)
    /// can't be loaded. With `cfg.tls = None` this never fails.
    pub fn new(
        addrs: Arc<[SocketAddr]>,
        breakers: Arc<[Breaker]>,
        cfg: ProbeConfig,
    ) -> Result<Self, crate::TlsLoadError> {
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(cfg.timeout));
        let connector = crate::build_https_connector(http, cfg.tls.clone())?;
        let client = Client::builder(TokioExecutor::new()).build(connector);
        let scheme = if cfg.tls.is_some() { "https" } else { "http" };
        Ok(Self {
            addrs,
            breakers,
            cfg,
            client,
            scheme,
        })
    }

    /// Drive the probe loops until `cancel.load(Acquire)` returns true.
    /// The returned future drives one task per addr concurrently.
    pub async fn run(&self, cancel: Arc<std::sync::atomic::AtomicBool>) {
        let mut tasks = Vec::with_capacity(self.addrs.len());
        for (idx, addr) in self.addrs.iter().enumerate() {
            if idx >= self.breakers.len() {
                continue;
            }
            let addr = *addr;
            let breakers = Arc::clone(&self.breakers);
            let client = self.client.clone();
            let cfg = self.cfg.clone();
            let scheme = self.scheme;
            let cancel = Arc::clone(&cancel);
            tasks.push(tokio::spawn(async move {
                run_one(addr, breakers, idx, client, cfg, scheme, cancel).await;
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    }
}

async fn run_one(
    addr: SocketAddr,
    breakers: Arc<[Breaker]>,
    idx: usize,
    client: ProbeClient,
    cfg: ProbeConfig,
    scheme: &'static str,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) {
    let breaker = &breakers[idx];
    let mut consecutive_fail = 0u32;
    let mut consecutive_ok = 0u32;
    while !cancel.load(std::sync::atomic::Ordering::Acquire) {
        let healthy = probe_once(&client, scheme, addr, &cfg.path, cfg.timeout).await;
        if healthy {
            consecutive_fail = 0;
            consecutive_ok = consecutive_ok.saturating_add(1);
            if consecutive_ok >= cfg.healthy_threshold {
                breaker.on_success();
            }
        } else {
            consecutive_ok = 0;
            consecutive_fail = consecutive_fail.saturating_add(1);
            if consecutive_fail >= cfg.unhealthy_threshold {
                breaker.on_failure();
            }
        }
        // Sleep with a cancellation-aware loop so SIGTERM does not
        // wait a whole `interval` before unblocking.
        let mut remaining = cfg.interval;
        let step = Duration::from_millis(250);
        while !cancel.load(std::sync::atomic::Ordering::Acquire) && !remaining.is_zero() {
            let s = step.min(remaining);
            tokio::time::sleep(s).await;
            remaining = remaining.saturating_sub(s);
        }
    }
}

async fn probe_once(
    client: &ProbeClient,
    scheme: &str,
    addr: SocketAddr,
    path: &str,
    timeout: Duration,
) -> bool {
    let path_normalized = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let Ok(uri) = format!("{scheme}://{addr}{path_normalized}").parse::<http::Uri>() else {
        return false;
    };
    let Ok(req) = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .body(Empty::<Bytes>::new())
    else {
        return false;
    };
    let send = client.request(req);
    match tokio::time::timeout(timeout, send).await {
        Ok(Ok(resp)) => {
            // Capture the status before the body move. Treat any 2xx
            // as healthy; 3xx/4xx/5xx mean "the backend isn't ready
            // for traffic" for an unauthenticated probe.
            let healthy = resp.status().is_success();
            // Drain the body so the connection returns to the pool.
            let _ = resp.into_body().collect().await;
            healthy
        }
        Ok(Err(_)) | Err(_) => false,
    }
}
