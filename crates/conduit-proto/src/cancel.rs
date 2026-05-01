//! Cancellation token: a runtime-agnostic `Arc<AtomicBool>` plus a
//! polling future. Cancellation is part of the request/response stream
//! contract — either side may flip the flag to signal "stop", and any
//! polling primitive (the body trait, an upstream timer, a higher-layer
//! deadline) checks it.
//!
//! # Why not `tokio_util::sync::CancellationToken`?
//!
//! `tokio_util` is a hard dependency on `tokio`. The proto layer is
//! runtime-agnostic; pulling tokio in here would prevent the monoio
//! backend from compiling without the `runtime-tokio` feature. The
//! polling shape we need is small enough that rolling our own keeps
//! the dependency graph honest.
//!
//! # Why not a `Notify`-style waker list?
//!
//! Every wake notification a proto-layer cancel could fire is already
//! waking the body's `poll_frame` future — the body's runtime polls it
//! on its own schedule. A waker list here would race with the runtime's
//! waker list and add a parasitic edge. Polling the flag from
//! `poll_frame` (or from a layer-4 timeout) is the cheap right thing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A clone-cheap cancellation handle.
///
/// All clones share one `AtomicBool`. `cancel()` is idempotent and
/// monotonic: once cancelled, always cancelled.
#[derive(Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl std::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    /// Build a fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Has anyone called [`Cancellation::cancel`] on a clone of this
    /// token? Uses `Acquire` ordering, pairing with the `Release` in
    /// `cancel`.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Flip the flag. Idempotent; subsequent calls are no-ops.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Convenience: return [`CancellationError::Cancelled`] if the token
    /// has already been flipped, otherwise `Ok(())`. Useful at the top
    /// of a poll function to fast-fail.
    pub fn check(&self) -> Result<(), CancellationError> {
        if self.is_cancelled() {
            Err(CancellationError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Error type returned by [`Cancellation::check`] and surfaced through
/// body / lifecycle errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CancellationError {
    /// The associated [`Cancellation`] has been flipped.
    #[error("operation cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_is_not_cancelled() {
        let c = Cancellation::new();
        assert!(!c.is_cancelled());
        assert!(c.check().is_ok());
    }

    #[test]
    fn cancel_propagates_to_clones() {
        let a = Cancellation::new();
        let b = a.clone();
        let c = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        assert!(c.is_cancelled());
        assert_eq!(c.check(), Err(CancellationError::Cancelled));
    }

    #[test]
    fn cancel_is_idempotent() {
        let c = Cancellation::new();
        c.cancel();
        c.cancel();
        c.cancel();
        assert!(c.is_cancelled());
    }

    #[test]
    fn cancel_is_monotonic() {
        // Once flipped, the flag never goes back. There is no reset API
        // by design — the contract is "cancellation is final."
        let c = Cancellation::new();
        c.cancel();
        // The struct intentionally exposes no `reset`; this test just
        // confirms the absence at the API level.
        assert!(c.is_cancelled());
    }

    #[test]
    fn debug_includes_state() {
        let c = Cancellation::new();
        let s = format!("{c:?}");
        assert!(s.contains("cancelled: false"), "got {s:?}");
        c.cancel();
        let s = format!("{c:?}");
        assert!(s.contains("cancelled: true"), "got {s:?}");
    }
}
