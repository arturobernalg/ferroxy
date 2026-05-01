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

/// Map an HTTP status code to its histogram bucket index, or `None`
/// for statuses outside the 2xx..5xx tracked range. Indexed:
/// 0 = 2xx, 1 = 3xx, 2 = 4xx, 3 = 5xx.
fn class_index(status: u16) -> Option<usize> {
    match status / 100 {
        2 => Some(0),
        3 => Some(1),
        4 => Some(2),
        5 => Some(3),
        _ => None,
    }
}

/// Status-class label strings used in Prometheus exposition.
const CLASS_LABELS: [&str; 4] = ["2xx", "3xx", "4xx", "5xx"];

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
    /// Cumulative request duration in microseconds, summed across
    /// every observation regardless of status class. Paired with
    /// `requests_total` to compute the histogram's average.
    pub request_duration_us_sum: AtomicU64,
    /// Cumulative-count buckets matching `LATENCY_BUCKETS_S`,
    /// summed across every observation. Reading at index `i` gives
    /// the count of requests that completed in `<= LATENCY_BUCKETS_S[i]`
    /// seconds.
    request_duration_buckets: [AtomicU64; LATENCY_BUCKETS_S.len()],
    /// Per-status-class duration histograms. Each entry holds its
    /// own bucket array + sum, so dashboards can split p99 of
    /// successes vs p99 of errors without the operator doing
    /// `PromQL` math. Indexed: 0 = 2xx, 1 = 3xx, 2 = 4xx, 3 = 5xx.
    request_duration_by_class: [ClassHistogram; 4],
}

/// One status-class histogram. Same shape as the global one but
/// filtered to a single status-class.
#[derive(Debug, Default)]
struct ClassHistogram {
    sum_us: AtomicU64,
    buckets: [AtomicU64; LATENCY_BUCKETS_S.len()],
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
    /// every cumulative bucket whose `le` is `>=` the observed
    /// duration in *both* the global histogram and the per-class
    /// histogram for the response status. Statuses outside 2xx-5xx
    /// (e.g. 1xx informationals) are recorded only in the global
    /// histogram.
    pub fn observe_request_duration(&self, elapsed: Duration, status: u16) {
        let secs = elapsed.as_secs_f64();
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);

        // Global histogram (every observation).
        for (i, le) in LATENCY_BUCKETS_S.iter().enumerate() {
            if secs <= *le {
                self.request_duration_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.request_duration_us_sum
            .fetch_add(us, Ordering::Relaxed);

        // Per-class histogram. 1xx and unknown classes (≥ 600) skip
        // this; they're already counted in the global one.
        if let Some(idx) = class_index(status) {
            let h = &self.request_duration_by_class[idx];
            for (i, le) in LATENCY_BUCKETS_S.iter().enumerate() {
                if secs <= *le {
                    h.buckets[i].fetch_add(1, Ordering::Relaxed);
                }
            }
            h.sum_us.fetch_add(us, Ordering::Relaxed);
        }
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
        if let Some(idx) = class_index(status) {
            let counter = match idx {
                0 => &self.responses_2xx,
                1 => &self.responses_3xx,
                2 => &self.responses_4xx,
                _ => &self.responses_5xx,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
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

        // Per-status-class histogram: one Prometheus histogram metric
        // with a `class` label so dashboards can query
        //   histogram_quantile(0.99, sum by (le) (rate(...{class="2xx"})))
        // for "p99 of successful responses" without summing across
        // classes by hand.
        out.push_str(
            "# HELP conduit_request_duration_seconds_by_class Latency split by status class.\n\
             # TYPE conduit_request_duration_seconds_by_class histogram\n",
        );
        for (idx, class) in CLASS_LABELS.iter().enumerate() {
            let h = &self.request_duration_by_class[idx];
            let mut class_total = 0u64;
            for (i, le) in LATENCY_BUCKETS_S.iter().enumerate() {
                let count = load(&h.buckets[i]);
                if i + 1 == LATENCY_BUCKETS_S.len() {
                    class_total = count.max(class_total);
                }
                let _ = writeln!(
                    out,
                    "conduit_request_duration_seconds_by_class_bucket{{class=\"{class}\",le=\"{le}\"}} {count}",
                );
            }
            // Prometheus's `+Inf` count must equal the total
            // observation count; we feed that from the per-class
            // counter rather than back-summing buckets.
            let class_count = match idx {
                0 => load(&self.responses_2xx),
                1 => load(&self.responses_3xx),
                2 => load(&self.responses_4xx),
                _ => load(&self.responses_5xx),
            };
            class_total = class_count.max(class_total);
            let class_micros = load(&h.sum_us);
            #[allow(clippy::cast_precision_loss)]
            let class_seconds = class_micros as f64 / 1_000_000.0;
            let _ = writeln!(
                out,
                "conduit_request_duration_seconds_by_class_bucket{{class=\"{class}\",le=\"+Inf\"}} {class_total}\n\
                 conduit_request_duration_seconds_by_class_count{{class=\"{class}\"}} {class_total}\n\
                 conduit_request_duration_seconds_by_class_sum{{class=\"{class}\"}} {class_seconds}",
            );
        }
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
        // 5ms 200 request → falls into le=0.005 and every larger bucket.
        m.observe_request_start();
        m.observe_status(200);
        m.observe_request_duration(Duration::from_millis(5), 200);
        // 200ms 200 request → falls into le=0.25 and larger.
        m.observe_request_start();
        m.observe_status(200);
        m.observe_request_duration(Duration::from_millis(200), 200);
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
    fn per_class_histogram_splits_2xx_from_5xx() {
        let m = MetricsHandle::new();
        // Two successful requests + one server error, distinct latencies.
        m.observe_status(200);
        m.observe_request_duration(Duration::from_millis(2), 200);
        m.observe_status(204);
        m.observe_request_duration(Duration::from_millis(8), 204);
        m.observe_status(503);
        m.observe_request_duration(Duration::from_millis(900), 503);

        let out = m.render();
        // Two 2xx observations in 0.01 (and beyond).
        assert!(out.contains(
            "conduit_request_duration_seconds_by_class_bucket{class=\"2xx\",le=\"0.01\"} 2"
        ));
        // Zero 5xx in 0.01 — the 5xx is at 0.9.
        assert!(out.contains(
            "conduit_request_duration_seconds_by_class_bucket{class=\"5xx\",le=\"0.01\"} 0"
        ));
        // The 5xx lands in le=1.
        assert!(out.contains(
            "conduit_request_duration_seconds_by_class_bucket{class=\"5xx\",le=\"1\"} 1"
        ));
        // 2xx count + 5xx count.
        assert!(out.contains("conduit_request_duration_seconds_by_class_count{class=\"2xx\"} 2"));
        assert!(out.contains("conduit_request_duration_seconds_by_class_count{class=\"5xx\"} 1"));
        // 2xx sum is ~0.010s; 5xx sum is ~0.900s.
        let two_xx_sum = out
            .lines()
            .find(|l| l.starts_with("conduit_request_duration_seconds_by_class_sum{class=\"2xx\"}"))
            .expect("2xx sum");
        let five_xx_sum = out
            .lines()
            .find(|l| l.starts_with("conduit_request_duration_seconds_by_class_sum{class=\"5xx\"}"))
            .expect("5xx sum");
        let parse_sum = |l: &str| -> f64 { l.split_whitespace().last().unwrap().parse().unwrap() };
        assert!((0.009..=0.012).contains(&parse_sum(two_xx_sum)));
        assert!((0.85..=0.92).contains(&parse_sum(five_xx_sum)));
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
