//! conduit-upstream — egress (upstream-side) HTTP client with retry.
//!
//! # What this crate does
//!
//! [`Upstream`] holds an `hyper_util::client::legacy::Client` plus a
//! [`RetryPolicy`]. [`Upstream::forward`] sends a request, then retries
//! according to the policy on configured status codes or connect errors.
//!
//! The upstream is keyed implicitly by the request's `Authority`; the
//! underlying hyper-util client maintains an H1 connection pool per
//! `(authority, scheme)` pair behind the scenes.
//!
//! # What this crate does not do (yet)
//!
//! - **Per-worker sharding.** Charter rule: connection pool sharded
//!   per worker, no `Arc<Mutex<…>>` on the hot path. The hyper-util
//!   `legacy::Client` is internally shared and uses tokio's
//!   synchronization primitives. We accept this as an interim
//!   implementation; the sharded replacement lands when phase 11.5
//!   profiling shows pool synchronisation in the top hot functions.
//! - **Active health checks** — the `[upstream.health_check]` config
//!   block is parsed but no probes run yet. P4.x.
//! - **Passive ejection** on N consecutive failures. P4.x.
//! - **Circuit breaker** state machine. P4.x.
//!
//! These deviations are surfaced in `phase4_deviations` in project memory.

#![deny(missing_docs)]

mod breaker;
mod health;
mod retry;

pub use breaker::{Breaker, BreakerConfig, BreakerState, Decision as BreakerDecision};
pub use health::{Probe, ProbeConfig};
pub use retry::{RetryDecision, RetryPolicy};

use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use conduit_proto::{BodyBytes, Request, Response};

/// Errors surfaced from [`Upstream::forward`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForwardError {
    /// hyper-util client could not connect to the upstream, or the
    /// connection broke before a complete response was received.
    /// `connect_error == true` if the error happened during the
    /// connect-or-handshake phase, before any byte of the request was
    /// sent — used by [`RetryPolicy`] to decide whether retry is safe.
    #[error("upstream client error (connect_error = {connect_error})")]
    Client {
        /// Underlying error.
        #[source]
        source: hyper_util::client::legacy::Error,
        /// Whether the failure was during the connect phase. The
        /// `is_connect()` predicate from hyper-util is the source of
        /// truth.
        connect_error: bool,
    },

    /// Failed to buffer the request body before sending. Bodies are
    /// buffered so retries can replay them; a buffering failure is
    /// pre-IO and not retryable.
    #[error("upstream request body buffering failed")]
    Body(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// All retry attempts were exhausted without producing a response
    /// the policy considers final. The last upstream status code is
    /// reported for observability; the body has been discarded.
    #[error("retries exhausted; last status was {last_status}")]
    RetriesExhausted {
        /// Status code of the most recent attempt.
        last_status: http::StatusCode,
    },

    /// The upstream's circuit breaker is open; the request was
    /// rejected without attempting a connection. The breaker resets
    /// when the cooldown elapses and a probe succeeds.
    #[error("upstream circuit breaker is open")]
    BreakerOpen,
}

/// Upstream holding a shared H1 client + retry policy + the list of
/// backend addresses to dial.
///
/// Cheap to clone — the inner `Client` is `Arc`-backed by hyper-util,
/// and `addrs` is shared via `Arc` so per-request access is a refcount
/// bump.
///
/// Load balancing across `addrs` is round-robin via an `AtomicUsize`
/// counter shared between clones — this is the only piece of shared
/// mutable state on the per-request path. Wrapping a counter in
/// `AtomicUsize::fetch_add` is one cycle on x86 with the relaxed
/// ordering used here; well below the per-request budget. Other
/// algorithms (least-connections, P2C, consistent-hash) land in
/// later cleanup; the field-level cost stays the same.
#[derive(Clone)]
pub struct Upstream {
    client: Client<hyper_util::client::legacy::connect::HttpConnector, BodyOf<Bytes>>,
    retry: RetryPolicy,
    addrs: std::sync::Arc<[std::net::SocketAddr]>,
    next: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// One passive circuit breaker per backend address, indexed in
    /// parallel with `addrs`. A bad backend is ejected from the
    /// rotation without taking down the rest of the pool. Shared via
    /// `Arc` so all clones see the same trip state — failures
    /// observed on one worker influence the next request's pick on
    /// every worker.
    breakers: std::sync::Arc<[Breaker]>,
}

/// Type alias for the body type our upstream client uses for requests.
/// `http_body_util::Full` is fine here because P4 forwards already-
/// in-memory bodies; streaming forwarding is a P5/P8 concern.
pub type BodyOf<D> = http_body_util::Full<D>;

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Upstream {
    /// Build a new upstream with no backend addresses. Forwarding to
    /// such an upstream falls back to whatever URI the request
    /// already carries (used by tests that drive the upstream
    /// directly with a fully-qualified URI).
    pub fn new(retry: RetryPolicy) -> Self {
        Self::with_addrs(retry, Vec::new())
    }

    /// Build with the supplied backend addresses. Requests round-robin
    /// across `addrs` via an `AtomicUsize` counter shared by all
    /// clones of this `Upstream`. The breaker uses default config —
    /// to override it at construction time use [`Upstream::with_breaker`].
    pub fn with_addrs(retry: RetryPolicy, addrs: Vec<std::net::SocketAddr>) -> Self {
        Self::with_breaker(retry, addrs, BreakerConfig::default())
    }

    /// Build with explicit breaker configuration. A `threshold` of 0
    /// disables the breakers entirely (every addr always passes).
    pub fn with_breaker(
        retry: RetryPolicy,
        addrs: Vec<std::net::SocketAddr>,
        breaker: BreakerConfig,
    ) -> Self {
        Self::with_options(retry, addrs, breaker, None)
    }

    /// Full constructor: retry policy, addrs, breaker config, and
    /// optional per-upstream connect timeout. The connect timeout
    /// applies to the TCP connect to a single backend address; the
    /// per-route total timeout (P5.x) is a separate, larger budget
    /// covering connect + send + recv.
    pub fn with_options(
        retry: RetryPolicy,
        addrs: Vec<std::net::SocketAddr>,
        breaker: BreakerConfig,
        connect_timeout: Option<Duration>,
    ) -> Self {
        let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
        if let Some(d) = connect_timeout {
            connector.set_connect_timeout(Some(d));
        }
        let client = Client::builder(TokioExecutor::new()).build(connector);
        // One breaker per addr. Vec → Arc<[T]> avoids per-request
        // refcount work; the breakers themselves are interior-mutable.
        let breakers: Vec<Breaker> = (0..addrs.len()).map(|_| Breaker::new(breaker)).collect();
        Self {
            client,
            retry,
            addrs: addrs.into(),
            next: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            breakers: breakers.into(),
        }
    }

    /// Pick the next backend address. Starts the round-robin scan at
    /// `next.fetch_add(1)` and walks at most `addrs.len()` positions
    /// looking for the first whose breaker is not Open. Returns the
    /// chosen address and its index (so [`Upstream::forward`] knows
    /// which breaker to update on the outcome).
    ///
    /// Returns `None` if every backend's breaker is Open — the caller
    /// surfaces this as [`ForwardError::BreakerOpen`].
    fn pick_addr(&self) -> Option<(std::net::SocketAddr, usize)> {
        if self.addrs.is_empty() {
            return None;
        }
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let len = self.addrs.len();
        for offset in 0..len {
            let idx = (start.wrapping_add(offset)) % len;
            if matches!(self.breakers[idx].check(), BreakerDecision::Allow) {
                return Some((self.addrs[idx], idx));
            }
        }
        None
    }

    /// Backend addresses (read-only).
    pub fn addrs(&self) -> &[std::net::SocketAddr] {
        &self.addrs
    }

    /// Cheaply-cloneable handle to the address array. Used by the
    /// probe builder to share the same backing storage as the hot
    /// path; `Arc<[T]>::clone` is one refcount bump.
    pub fn addrs_arc(&self) -> std::sync::Arc<[std::net::SocketAddr]> {
        std::sync::Arc::clone(&self.addrs)
    }

    /// Cheaply-cloneable handle to the per-addr breaker array. Used
    /// by the probe builder so probe outcomes update the same
    /// breakers the request hot path consults.
    pub fn breakers_arc(&self) -> std::sync::Arc<[Breaker]> {
        std::sync::Arc::clone(&self.breakers)
    }

    /// Aggregate breaker state across every backend address.
    ///
    /// - `Closed` — every breaker is closed (or no addrs configured).
    /// - `Open` — every breaker is open.
    /// - `HalfOpen` — at least one is in half-open or breakers
    ///   disagree (some closed, some open). The middle state means
    ///   "the upstream is partially degraded"; admin dashboards
    ///   surface it accordingly.
    pub fn breaker_state(&self) -> breaker::BreakerState {
        if self.breakers.is_empty() {
            return breaker::BreakerState::Closed;
        }
        let mut closed = 0usize;
        let mut open = 0usize;
        for b in self.breakers.iter() {
            match b.snapshot() {
                breaker::BreakerState::Closed => closed += 1,
                breaker::BreakerState::Open => open += 1,
                breaker::BreakerState::HalfOpen => {}
            }
        }
        if closed == self.breakers.len() {
            breaker::BreakerState::Closed
        } else if open == self.breakers.len() {
            breaker::BreakerState::Open
        } else {
            breaker::BreakerState::HalfOpen
        }
    }

    /// Send `req` upstream and return the response. Body is buffered in
    /// memory between attempts so retries can replay it; this is fine
    /// for P4's <1 KB request-body target workload but is the major
    /// reason streaming retries are a P8 concern, not a P4 one.
    ///
    /// On success returns the upstream `Response<Incoming>`. On exhaust
    /// returns [`ForwardError::RetriesExhausted`]; on a connect error
    /// where retries do not apply, returns [`ForwardError::Client`].
    pub async fn forward<B>(
        &self,
        req: Request<B>,
    ) -> Result<Response<hyper::body::Incoming>, ForwardError>
    where
        B: BodyBytes + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        // Buffer the body so retries can replay it. P4 target workload
        // has <1 KB request bodies p95; the buffering cost is
        // negligible.
        let (mut parts, body) = req.into_parts();

        // Translate to HTTP/1.1 for egress regardless of how the
        // request arrived. hyper-util's H1 pool sends HTTP/1.1; an
        // H2 ingress request whose version is still `HTTP/2.0` would
        // confuse hyper at serialization time. P8 charter goal: H2
        // ingress → H1 egress is the production reverse-proxy path.
        // H2 hop-by-hop headers (`te`, `connection`, etc.) are
        // already stripped by hyper's H2 server on the way in, so we
        // do not re-strip them here.
        parts.version = http::Version::HTTP_11;
        // Strip the inbound `host` header so hyper-util's H1 client
        // can derive Host from the rewritten URI authority below.
        // H2 ingress never sends a `host` header (it uses :authority,
        // which hyper turns into the URI), so this is a no-op for H2;
        // for H1 ingress it ensures a consistent Host across retries.
        parts.headers.remove(http::header::HOST);

        // Rewrite the URI to an absolute http:// URL pointing at the
        // backend chosen by our load balancer. `pick_addr` skips
        // breaker-open backends; if every backend is open the request
        // fails fast with BreakerOpen. With no configured addrs we
        // fall back to whatever URI the request already carries (used
        // by the test suite that drives Upstream with a fully-formed
        // URI directly).
        let addr_idx: Option<usize> = if self.addrs.is_empty() {
            None
        } else {
            match self.pick_addr() {
                Some((addr, idx)) => {
                    let path_and_query = parts
                        .uri
                        .path_and_query()
                        .map_or("/", http::uri::PathAndQuery::as_str)
                        .to_owned();
                    let new_uri: http::Uri = format!("http://{addr}{path_and_query}")
                        .parse()
                        .map_err(|e: http::uri::InvalidUri| {
                            ForwardError::Body(Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                e.to_string(),
                            )))
                        })?;
                    parts.uri = new_uri;
                    Some(idx)
                }
                None => return Err(ForwardError::BreakerOpen),
            }
        };

        let bytes = body
            .collect()
            .await
            .map_err(|e| ForwardError::Body(e.into()))?
            .to_bytes();

        let mut last_status = http::StatusCode::INTERNAL_SERVER_ERROR;
        for attempt in 0..self.retry.attempts.max(1) {
            let body = http_body_util::Full::new(bytes.clone());
            let req = http::Request::from_parts(parts.clone(), body);

            match self.client.request(req).await {
                Ok(resp) => {
                    last_status = resp.status();
                    match self.retry.decide_status(last_status, attempt) {
                        RetryDecision::Stop => {
                            // 5xx responses still count as failures
                            // for breaker purposes — the upstream is
                            // up but not serving. 2xx/3xx/4xx all reset
                            // the breaker.
                            self.observe_breaker_outcome(addr_idx, !last_status.is_server_error());
                            return Ok(resp);
                        }
                        RetryDecision::Retry => {
                            tracing::debug!(
                                attempt = attempt + 1,
                                status = last_status.as_u16(),
                                "upstream returned retryable status; will retry"
                            );
                        }
                    }
                }
                Err(e) => {
                    let connect_error = e.is_connect();
                    if connect_error
                        && self.retry.on_connect_error
                        && attempt + 1 < self.retry.attempts
                    {
                        tracing::debug!(
                            attempt = attempt + 1,
                            "upstream connect error; will retry"
                        );
                        continue;
                    }
                    self.observe_breaker_outcome(addr_idx, false);
                    return Err(ForwardError::Client {
                        source: e,
                        connect_error,
                    });
                }
            }
        }
        // Loop exhausted retries without `Stop`-ing. Treat as failure
        // for breaker purposes too.
        self.observe_breaker_outcome(addr_idx, false);
        Err(ForwardError::RetriesExhausted { last_status })
    }

    /// Record `success` (or failure) against the breaker for the addr
    /// index that `pick_addr` returned. Indexless requests (test-only:
    /// no addrs configured) are ignored.
    fn observe_breaker_outcome(&self, idx: Option<usize>, success: bool) {
        if let Some(i) = idx {
            if let Some(b) = self.breakers.get(i) {
                if success {
                    b.on_success();
                } else {
                    b.on_failure();
                }
            }
        }
    }
}

/// Default request timeout when none is specified. P5 (lifecycle) will
/// thread real per-route timeouts through; P4 keeps it loose.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    /// Strip the index off `pick_addr` for tests that only care
    /// about the chosen address.
    fn pick_a(u: &Upstream) -> Option<std::net::SocketAddr> {
        u.pick_addr().map(|(a, _)| a)
    }

    #[test]
    fn round_robin_picks_addrs_in_order() {
        let u = Upstream::with_addrs(
            RetryPolicy::none(),
            vec![sa("10.0.0.1:80"), sa("10.0.0.2:80"), sa("10.0.0.3:80")],
        );
        let picks: Vec<_> = (0..6).map(|_| pick_a(&u).unwrap()).collect();
        assert_eq!(
            picks,
            vec![
                sa("10.0.0.1:80"),
                sa("10.0.0.2:80"),
                sa("10.0.0.3:80"),
                sa("10.0.0.1:80"),
                sa("10.0.0.2:80"),
                sa("10.0.0.3:80"),
            ]
        );
    }

    #[test]
    fn empty_addrs_returns_none() {
        let u = Upstream::new(RetryPolicy::none());
        assert!(u.pick_addr().is_none());
    }

    #[test]
    fn round_robin_state_shared_across_clones() {
        // Clones share the round-robin counter (Arc<AtomicUsize>)
        // so that every worker thread sees a fair distribution.
        let u = Upstream::with_addrs(
            RetryPolicy::none(),
            vec![sa("10.0.0.1:80"), sa("10.0.0.2:80")],
        );
        let u2 = u.clone();
        assert_eq!(pick_a(&u), Some(sa("10.0.0.1:80")));
        assert_eq!(pick_a(&u2), Some(sa("10.0.0.2:80")));
        assert_eq!(pick_a(&u), Some(sa("10.0.0.1:80")));
        assert_eq!(pick_a(&u2), Some(sa("10.0.0.2:80")));
    }

    #[test]
    fn pick_addr_skips_breaker_open_addrs() {
        let u = Upstream::with_breaker(
            RetryPolicy::none(),
            vec![sa("10.0.0.1:80"), sa("10.0.0.2:80"), sa("10.0.0.3:80")],
            BreakerConfig {
                threshold: 1,
                cooldown: Duration::from_secs(60),
            },
        );
        // Trip addr 1 (10.0.0.2:80).
        u.breakers[1].on_failure();
        // Round-robin starts at 0 → picks .1; next pick should skip
        // the open .2 and land on .3.
        assert_eq!(pick_a(&u), Some(sa("10.0.0.1:80")));
        assert_eq!(pick_a(&u), Some(sa("10.0.0.3:80")));
        // Trip everything else; pick_addr returns None.
        u.breakers[0].on_failure();
        u.breakers[2].on_failure();
        assert!(u.pick_addr().is_none());
    }
}
