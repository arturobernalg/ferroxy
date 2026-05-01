//! Lock-free metric counters fed by the data plane.
//!
//! `MetricsHandle` is the single shared metrics sink. Every per-request
//! handler holds an `Arc<MetricsHandle>` and bumps counters via
//! `fetch_add(1, Relaxed)`; the admin server holds the same handle and
//! reads them on every `/metrics` scrape.
//!
//! `Relaxed` ordering is sufficient — counters are monotonic and the
//! exact interleaving across cores has no observable effect on the
//! exposition format. Charter rule: no `Mutex` on the hot path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free counter family fed by the data plane.
///
/// Construct one `Arc<MetricsHandle>` at startup, hand a clone to the
/// admin server, and hand a clone to every per-request handler. The
/// `Arc` clone is a refcount bump; the per-request bookkeeping is one
/// `fetch_add` per outcome.
#[derive(Debug, Default)]
pub struct MetricsHandle {
    /// Total requests observed by the data plane (success + error).
    pub requests_total: AtomicU64,
    /// Requests that resolved to no matching route (404 to client).
    pub requests_no_route: AtomicU64,
    /// Requests that referenced an unregistered upstream (500 to
    /// client; should be impossible after config validation).
    pub requests_upstream_unknown: AtomicU64,
    /// Requests where the upstream forward failed (502 to client).
    pub requests_upstream_failed: AtomicU64,
    /// Requests that completed with a 2xx status code.
    pub responses_2xx: AtomicU64,
    /// Requests that completed with a 3xx status code.
    pub responses_3xx: AtomicU64,
    /// Requests that completed with a 4xx status code.
    pub responses_4xx: AtomicU64,
    /// Requests that completed with a 5xx status code.
    pub responses_5xx: AtomicU64,
}

impl MetricsHandle {
    /// Create an empty handle. Use `Arc::new(MetricsHandle::new())` at
    /// startup; clone the `Arc` per consumer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump `requests_total` once at the start of every proxied request.
    pub fn observe_request_start(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a "no matching route" outcome for the just-observed request.
    pub fn observe_no_route(&self) {
        self.requests_no_route.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an "upstream not registered" outcome.
    pub fn observe_upstream_unknown(&self) {
        self.requests_upstream_unknown
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record an "upstream forward failed" outcome.
    pub fn observe_upstream_failed(&self) {
        self.requests_upstream_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record the response status class. Pass the numeric status code;
    /// the helper bins it into 2xx/3xx/4xx/5xx. 1xx is not tracked
    /// (we do not surface informational responses today).
    pub fn observe_status(&self, status: u16) {
        let counter = match status / 100 {
            2 => &self.responses_2xx,
            3 => &self.responses_3xx,
            4 => &self.responses_4xx,
            5 => &self.responses_5xx,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the counter family as Prometheus 0.0.4 text exposition.
    /// Concatenated with the synthetic uptime / `build_info` gauges by
    /// the admin server before being served on `/metrics`.
    pub fn render(&self) -> String {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "# HELP conduit_requests_total Total requests observed by the data plane.\n\
             # TYPE conduit_requests_total counter\n\
             conduit_requests_total {}\n\
             # HELP conduit_requests_no_route_total Requests that did not match any route.\n\
             # TYPE conduit_requests_no_route_total counter\n\
             conduit_requests_no_route_total {}\n\
             # HELP conduit_requests_upstream_unknown_total Routes referencing an unregistered upstream.\n\
             # TYPE conduit_requests_upstream_unknown_total counter\n\
             conduit_requests_upstream_unknown_total {}\n\
             # HELP conduit_requests_upstream_failed_total Upstream forward failures.\n\
             # TYPE conduit_requests_upstream_failed_total counter\n\
             conduit_requests_upstream_failed_total {}\n\
             # HELP conduit_responses_total Responses by status class.\n\
             # TYPE conduit_responses_total counter\n\
             conduit_responses_total{{class=\"2xx\"}} {}\n\
             conduit_responses_total{{class=\"3xx\"}} {}\n\
             conduit_responses_total{{class=\"4xx\"}} {}\n\
             conduit_responses_total{{class=\"5xx\"}} {}\n",
            load(&self.requests_total),
            load(&self.requests_no_route),
            load(&self.requests_upstream_unknown),
            load(&self.requests_upstream_failed),
            load(&self.responses_2xx),
            load(&self.responses_3xx),
            load(&self.responses_4xx),
            load(&self.responses_5xx),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_zero() {
        let m = MetricsHandle::new();
        assert!(m.render().contains("conduit_requests_total 0"));
        assert!(m
            .render()
            .contains("conduit_responses_total{class=\"2xx\"} 0"));
    }

    #[test]
    fn observe_status_bins_correctly() {
        let m = MetricsHandle::new();
        m.observe_status(200);
        m.observe_status(204);
        m.observe_status(301);
        m.observe_status(404);
        m.observe_status(503);
        m.observe_status(100); // ignored
        let out = m.render();
        assert!(out.contains("conduit_responses_total{class=\"2xx\"} 2"));
        assert!(out.contains("conduit_responses_total{class=\"3xx\"} 1"));
        assert!(out.contains("conduit_responses_total{class=\"4xx\"} 1"));
        assert!(out.contains("conduit_responses_total{class=\"5xx\"} 1"));
    }

    #[test]
    fn outcome_counters_independent() {
        let m = MetricsHandle::new();
        m.observe_request_start();
        m.observe_no_route();
        m.observe_request_start();
        m.observe_upstream_failed();
        let out = m.render();
        assert!(out.contains("conduit_requests_total 2"));
        assert!(out.contains("conduit_requests_no_route_total 1"));
        assert!(out.contains("conduit_requests_upstream_failed_total 1"));
        assert!(out.contains("conduit_requests_upstream_unknown_total 0"));
    }
}
