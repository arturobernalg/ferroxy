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

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Prometheus-default histogram bucket boundaries, in seconds. The
/// last "bucket" is `+Inf`, encoded as a wrapping bump of
/// `requests_total` (Prometheus's text format requires `+Inf` to be
/// the count of every observation, which is exactly what
/// `requests_total` already tracks).
const LATENCY_BUCKETS_S: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

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
    /// Cumulative request duration in microseconds. Paired with
    /// `requests_total` to compute the histogram's average; required
    /// by Prometheus exposition.
    pub request_duration_us_sum: AtomicU64,
    /// Cumulative-count buckets matching `LATENCY_BUCKETS_S`.
    /// Cumulative means each bucket counts every observation `<= le`,
    /// so reading at index `i` gives the count of requests that
    /// completed in `<= LATENCY_BUCKETS_S[i]` seconds.
    request_duration_buckets: [AtomicU64; LATENCY_BUCKETS_S.len()],
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

    /// Record the wall-clock duration of a completed request. Bumps
    /// every cumulative bucket whose `le` is `>=` the observed duration
    /// and adds the elapsed microseconds to `request_duration_us_sum`.
    pub fn observe_request_duration(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        for (i, le) in LATENCY_BUCKETS_S.iter().enumerate() {
            if secs <= *le {
                self.request_duration_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.request_duration_us_sum
            .fetch_add(us, Ordering::Relaxed);
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
        let mut out = format!(
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
        );
        out.push_str(
            "# HELP conduit_request_duration_seconds Wall-clock latency of proxied requests.\n\
             # TYPE conduit_request_duration_seconds histogram\n",
        );
        for (i, le) in LATENCY_BUCKETS_S.iter().enumerate() {
            let count = load(&self.request_duration_buckets[i]);
            let _ = writeln!(
                out,
                "conduit_request_duration_seconds_bucket{{le=\"{le}\"}} {count}",
            );
        }
        let total = load(&self.requests_total);
        let micros = load(&self.request_duration_us_sum);
        // 64-bit microseconds in an f64 second value is lossy past
        // ~285 years of cumulative latency; for any realistic uptime
        // the precision loss is below the histogram's resolution.
        #[allow(clippy::cast_precision_loss)]
        let seconds = micros as f64 / 1_000_000.0;
        let _ = writeln!(
            out,
            "conduit_request_duration_seconds_bucket{{le=\"+Inf\"}} {total}\n\
             conduit_request_duration_seconds_count {total}\n\
             conduit_request_duration_seconds_sum {seconds}",
        );
        out
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
    fn histogram_buckets_are_cumulative() {
        let m = MetricsHandle::new();
        // 5ms request → falls into le=0.005 and every larger bucket.
        m.observe_request_start();
        m.observe_request_duration(Duration::from_millis(5));
        // 200ms request → falls into le=0.25 and larger.
        m.observe_request_start();
        m.observe_request_duration(Duration::from_millis(200));
        let out = m.render();
        // Both observations are in the 0.25 bucket and beyond.
        assert!(out.contains("conduit_request_duration_seconds_bucket{le=\"0.25\"} 2"));
        // Only the 5ms one is in 0.005.
        assert!(out.contains("conduit_request_duration_seconds_bucket{le=\"0.005\"} 1"));
        // Nothing in 0.001.
        assert!(out.contains("conduit_request_duration_seconds_bucket{le=\"0.001\"} 0"));
        // +Inf must equal requests_total.
        assert!(out.contains("conduit_request_duration_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(out.contains("conduit_request_duration_seconds_count 2"));
        // Sum is 0.005 + 0.200 = 0.205.
        let line = out
            .lines()
            .find(|l| l.starts_with("conduit_request_duration_seconds_sum "))
            .expect("sum line");
        let value: f64 = line
            .trim_start_matches("conduit_request_duration_seconds_sum ")
            .parse()
            .expect("sum is numeric");
        assert!(
            (0.20..=0.21).contains(&value),
            "sum was {value}, expected ~0.205",
        );
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
