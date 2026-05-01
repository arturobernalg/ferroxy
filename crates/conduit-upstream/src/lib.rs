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

mod retry;

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
}

/// Upstream holding a shared H1 client + retry policy.
///
/// Cheap to clone — the inner `Client` is `Arc`-backed by hyper-util.
#[derive(Clone)]
pub struct Upstream {
    client: Client<hyper_util::client::legacy::connect::HttpConnector, BodyOf<Bytes>>,
    retry: RetryPolicy,
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
    /// Build a new upstream with the supplied retry policy. Connection
    /// pooling parameters use hyper-util's defaults; the
    /// `[upstream.pool]` config block is honored in a follow-up commit
    /// once the bridge to per-worker sharding lands (P4.x).
    pub fn new(retry: RetryPolicy) -> Self {
        let connector = hyper_util::client::legacy::connect::HttpConnector::new();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self { client, retry }
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
        let (parts, body) = req.into_parts();
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
                        RetryDecision::Stop => return Ok(resp),
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
                    return Err(ForwardError::Client {
                        source: e,
                        connect_error,
                    });
                }
            }
        }
        Err(ForwardError::RetriesExhausted { last_status })
    }
}

/// Default request timeout when none is specified. P5 (lifecycle) will
/// thread real per-route timeouts through; P4 keeps it loose.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
