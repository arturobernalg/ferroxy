//! Lock-free passive circuit breaker.
//!
//! `Breaker` tracks consecutive upstream failures with two atomics —
//! a failure counter and a "open until" deadline (millis since the
//! breaker's anchor `Instant`). When the counter reaches `threshold`
//! and the deadline is in the future, [`Breaker::check`] returns
//! [`Decision::Reject`] and the upstream forward fails fast without
//! taking out a connection.
//!
//! State machine:
//!
//! - **Closed** — `failures < threshold`. Requests pass through.
//! - **Open** — `failures >= threshold` and `now < open_until`. Requests
//!   are rejected immediately.
//! - **Half-open** — `failures >= threshold` and `now >= open_until`.
//!   Requests pass through. The first one that completes determines the
//!   next state: success → Closed; failure → Open with a fresh deadline.
//!
//! Charter rule: no `Mutex` on the hot path. The check + update is
//! three relaxed atomic operations.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Decision returned by [`Breaker::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Forward the request normally.
    Allow,
    /// Reject the request immediately; do not connect to the upstream.
    Reject,
}

/// Configuration for [`Breaker::new`]. `threshold = 0` disables the
/// breaker entirely (every check returns `Allow`).
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Number of consecutive failures that trip the breaker.
    pub threshold: u32,
    /// How long the breaker stays open after tripping. After this
    /// elapses the breaker enters half-open and the next request acts
    /// as a probe.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    /// Sensible production defaults: 5 consecutive failures, 30s
    /// cooldown. Operators override via the `[upstream.health_check]`
    /// config block once that lands.
    fn default() -> Self {
        Self {
            threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Lock-free passive circuit breaker. Cheap to clone (held inside an
/// `Arc`-able struct); each clone shares the same atomic state.
#[derive(Debug)]
pub struct Breaker {
    /// Threshold at which the breaker trips. `0` disables.
    threshold: u32,
    /// Cooldown the breaker stays open for after tripping, in
    /// milliseconds (kept as `u64` so we can stash it next to the
    /// counter in atomics without losing precision).
    cooldown_ms: u64,
    /// Anchor instant. `open_until_ms` and `now_ms()` are deltas from
    /// this point.
    anchor: Instant,
    /// Consecutive failures.
    failures: AtomicU32,
    /// Millis-since-`anchor` at which the breaker stops being open.
    /// Zero means never tripped (i.e. closed).
    open_until_ms: AtomicU64,
}

impl Breaker {
    /// Build a new breaker. With `threshold == 0` the breaker is a
    /// no-op (every `check` returns `Allow`).
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            threshold: cfg.threshold,
            cooldown_ms: u64::try_from(cfg.cooldown.as_millis()).unwrap_or(u64::MAX),
            anchor: Instant::now(),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Decide whether the next request can proceed. Cheap: two atomic
    /// loads in the rejected case, three in the accepted case.
    pub fn check(&self) -> Decision {
        if self.threshold == 0 {
            return Decision::Allow;
        }
        if self.failures.load(Ordering::Relaxed) < self.threshold {
            return Decision::Allow;
        }
        let open_until = self.open_until_ms.load(Ordering::Relaxed);
        if open_until == 0 || self.now_ms() >= open_until {
            // Tripped but cooldown elapsed → half-open. Allow this
            // request as a probe; on_success / on_failure will decide
            // the next state.
            Decision::Allow
        } else {
            Decision::Reject
        }
    }

    /// Record a successful upstream response. Resets the breaker to
    /// the closed state.
    pub fn on_success(&self) {
        if self.threshold == 0 {
            return;
        }
        // Order is significant: clear the deadline first so a
        // half-open success cannot race with a concurrent failure
        // observing `failures < threshold` and skipping the deadline
        // refresh. With both stores Relaxed the worst case is a
        // single spurious `Allow` followed by reconvergence.
        self.open_until_ms.store(0, Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed);
    }

    /// Record an upstream failure. If this is the failure that crosses
    /// the threshold, arm the cooldown deadline.
    pub fn on_failure(&self) {
        if self.threshold == 0 {
            return;
        }
        let prev = self.failures.fetch_add(1, Ordering::Relaxed);
        let new = prev.saturating_add(1);
        if new == self.threshold {
            // First failure to cross the threshold: set the deadline.
            // Subsequent failures inside an already-open window also
            // refresh the deadline so the breaker stays open under
            // sustained failure.
            let until = self.now_ms().saturating_add(self.cooldown_ms);
            self.open_until_ms.store(until, Ordering::Relaxed);
        } else if new > self.threshold {
            // Already open; refresh deadline.
            let until = self.now_ms().saturating_add(self.cooldown_ms);
            self.open_until_ms.store(until, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_zero_is_a_no_op() {
        let b = Breaker::new(BreakerConfig {
            threshold: 0,
            cooldown: Duration::from_secs(1),
        });
        for _ in 0..1000 {
            b.on_failure();
        }
        assert_eq!(b.check(), Decision::Allow);
    }

    #[test]
    fn opens_after_threshold_failures() {
        let b = Breaker::new(BreakerConfig {
            threshold: 3,
            cooldown: Duration::from_secs(60),
        });
        b.on_failure();
        b.on_failure();
        assert_eq!(b.check(), Decision::Allow);
        b.on_failure();
        assert_eq!(b.check(), Decision::Reject);
    }

    #[test]
    fn success_resets_breaker() {
        let b = Breaker::new(BreakerConfig {
            threshold: 2,
            cooldown: Duration::from_secs(60),
        });
        b.on_failure();
        b.on_failure();
        assert_eq!(b.check(), Decision::Reject);
        b.on_success();
        assert_eq!(b.check(), Decision::Allow);
    }

    #[test]
    fn cooldown_elapses_to_half_open() {
        let b = Breaker::new(BreakerConfig {
            threshold: 1,
            cooldown: Duration::from_millis(50),
        });
        b.on_failure();
        assert_eq!(b.check(), Decision::Reject);
        std::thread::sleep(Duration::from_millis(80));
        // Half-open: probe is allowed.
        assert_eq!(b.check(), Decision::Allow);
    }
}
