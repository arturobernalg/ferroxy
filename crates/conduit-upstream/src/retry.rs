//! Retry policy: status-code- and connect-error-driven retries with a
//! bounded attempt count.
//!
//! The policy is data — pure decisions over (status, attempt). The
//! actual retry loop lives in `Upstream::forward`. Splitting them
//! keeps the policy testable without spinning up upstream connections.

use http::StatusCode;

/// Retry decision returned by [`RetryPolicy::decide_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// The status code is final; return it to the caller.
    Stop,
    /// The status is in the retry-on list and we have attempts left;
    /// caller should issue another request.
    Retry,
}

/// Per-route retry rules. Constructed by `conduit-config` from the
/// `[[route]].retry` block; consumed by `Upstream::forward`.
#[derive(Debug, Clone, Default)]
pub struct RetryPolicy {
    /// Maximum total attempts (including the original try). 0 or 1
    /// means "no retry."
    pub attempts: u32,
    /// Status codes that trigger a retry. Empty = retry on no status.
    pub on_status: Vec<u16>,
    /// Whether to retry on connect-phase errors (the request never
    /// reached the upstream, so retry is safe).
    pub on_connect_error: bool,
}

impl RetryPolicy {
    /// Convenience: no retries.
    pub fn none() -> Self {
        Self::default()
    }

    /// Build from a `conduit_config::RetryPolicy` struct. Kept here
    /// rather than as a `From` impl on the config side to keep
    /// dependency direction pointing downward (upstream depends on
    /// config, not the other way round).
    #[must_use]
    pub fn from_config(cfg: &conduit_config::RetryPolicy) -> Self {
        Self {
            attempts: cfg.attempts,
            on_status: cfg.on_status.clone(),
            on_connect_error: cfg.on_connect_error,
        }
    }

    /// Decide whether a response with `status` after `attempt` should
    /// trigger a retry. `attempt` is zero-based: attempt 0 is the
    /// initial request, attempt 1 is the first retry, etc.
    ///
    /// Returns [`RetryDecision::Retry`] only if:
    ///   - `status` is in `on_status`, AND
    ///   - `attempt + 1 < attempts` (room for one more try).
    pub fn decide_status(&self, status: StatusCode, attempt: u32) -> RetryDecision {
        if !self.on_status.contains(&status.as_u16()) {
            return RetryDecision::Stop;
        }
        if attempt + 1 >= self.attempts {
            return RetryDecision::Stop;
        }
        RetryDecision::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_502_2x() -> RetryPolicy {
        RetryPolicy {
            attempts: 2,
            on_status: vec![502, 503, 504],
            on_connect_error: true,
        }
    }

    #[test]
    fn final_status_stops() {
        let p = policy_502_2x();
        assert_eq!(p.decide_status(StatusCode::OK, 0), RetryDecision::Stop);
        assert_eq!(
            p.decide_status(StatusCode::NOT_FOUND, 0),
            RetryDecision::Stop
        );
    }

    #[test]
    fn retryable_status_retries_when_attempts_available() {
        let p = policy_502_2x();
        assert_eq!(
            p.decide_status(StatusCode::BAD_GATEWAY, 0),
            RetryDecision::Retry
        );
        assert_eq!(
            p.decide_status(StatusCode::SERVICE_UNAVAILABLE, 0),
            RetryDecision::Retry
        );
    }

    #[test]
    fn retryable_status_stops_when_no_attempts_left() {
        let p = policy_502_2x();
        // attempts=2 means original (attempt 0) + 1 retry. After
        // attempt 1, no more retries.
        assert_eq!(
            p.decide_status(StatusCode::BAD_GATEWAY, 1),
            RetryDecision::Stop
        );
    }

    #[test]
    fn no_retry_policy_never_retries() {
        let p = RetryPolicy::none();
        assert_eq!(
            p.decide_status(StatusCode::BAD_GATEWAY, 0),
            RetryDecision::Stop
        );
    }

    #[test]
    fn attempts_zero_treated_as_no_retry() {
        let p = RetryPolicy {
            attempts: 0,
            on_status: vec![502],
            on_connect_error: false,
        };
        assert_eq!(
            p.decide_status(StatusCode::BAD_GATEWAY, 0),
            RetryDecision::Stop
        );
    }
}
