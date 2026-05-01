//! `TimedBody`: read-timeout wrapper around any
//! [`http_body::Body`].
//!
//! Wraps the upstream's response body so that a slow / silent
//! backend can't tie up a conduit connection forever. Each
//! `poll_frame` arms a `tokio::time::Sleep` for the configured
//! `read_timeout`; the timer is reset whenever a frame makes it
//! through. If the timer fires before the next frame, the body
//! resolves with `Self::Error` (a boxed `ReadTimeoutError`).
//!
//! Charter rule: per-leg read/write timeouts. Connect timeout is
//! configured on the `HttpConnector`; total timeout is enforced at
//! the dispatch level. This wrapper is the read-leg piece.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;

pin_project! {
    /// Body wrapper that fails fast when the inner body produces no
    /// frame for `timeout` seconds. The timer is per-poll: a frame
    /// resets it, an end-of-stream resets it (and never fires
    /// again), and pending state arms it.
    pub struct TimedBody<B> {
        #[pin]
        inner: B,
        timeout: Duration,
        #[pin]
        sleep: Option<tokio::time::Sleep>,
    }
}

impl<B> std::fmt::Debug for TimedBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom impl: B may not be Debug (Incoming isn't), and the
        // inner Sleep isn't Debug either. We surface only the
        // fields useful for telemetry (timeout) and finish_non_exhaustive
        // so future fields don't trip clippy::manual_debug.
        f.debug_struct("TimedBody")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<B> TimedBody<B> {
    /// Wrap `inner` with a per-frame read timeout. Constructing a
    /// `TimedBody` is allocation-free; the `Sleep` is registered
    /// only when the body is polled and discarded as soon as a
    /// frame returns.
    pub fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: None,
        }
    }
}

/// Error returned when no frame arrived within the configured
/// `read_timeout`. Wrapped in a boxed `Error` so it composes with
/// the `BoxBody`-style trait bounds elsewhere in conduit.
#[derive(Debug)]
pub struct ReadTimeout {
    /// The configured deadline that fired.
    pub timeout: Duration,
}

impl std::fmt::Display for ReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream did not send a frame within {:?}", self.timeout)
    }
}

impl std::error::Error for ReadTimeout {}

impl<B> Body for TimedBody<B>
where
    B: Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = B::Data;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut me = self.project();

        // Arm the timer the first time we're polled, or rearm after
        // it fired previously and we cleared it.
        if me.sleep.as_ref().as_pin_ref().is_none() {
            me.sleep.set(Some(tokio::time::sleep(*me.timeout)));
        }

        // Poll the inner body first. On any progress (frame, end,
        // error) clear the timer so the next call rearms fresh.
        match me.inner.as_mut().poll_frame(cx) {
            Poll::Ready(opt) => {
                me.sleep.set(None);
                return Poll::Ready(opt.map(|r| r.map_err(Into::into)));
            }
            Poll::Pending => {}
        }

        // Inner is pending. Has the timer fired?
        if let Some(sleep) = me.sleep.as_mut().as_pin_mut() {
            if sleep.poll(cx).is_ready() {
                let err: Box<dyn std::error::Error + Send + Sync> = Box::new(ReadTimeout {
                    timeout: *me.timeout,
                });
                // Drop the fired timer; we won't be polled again.
                me.sleep.set(None);
                return Poll::Ready(Some(Err(err)));
            }
        }

        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body::Frame;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Body that returns one data frame, then `Pending` forever.
    struct StuckBody {
        emitted: AtomicU8,
    }
    impl StuckBody {
        fn new() -> Self {
            Self {
                emitted: AtomicU8::new(0),
            }
        }
    }
    impl Body for StuckBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if self.emitted.fetch_add(1, Ordering::Relaxed) == 0 {
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"hi")))))
            } else {
                Poll::Pending
            }
        }
    }

    /// Drive the body to completion (or read-timeout error) using
    /// `http_body_util::BodyExt::collect` over real wall-clock time.
    /// 50ms timeout × 200ms wait → timer fires deterministically.
    #[tokio::test(flavor = "current_thread")]
    async fn timer_fires_when_inner_stalls() {
        use http_body_util::BodyExt;
        let body = TimedBody::new(StuckBody::new(), Duration::from_millis(50));
        let collected = body.collect().await;
        match collected {
            Err(e) => {
                let downcast = e.downcast::<ReadTimeout>();
                assert!(
                    downcast.is_ok(),
                    "expected ReadTimeout, got non-ReadTimeout error",
                );
            }
            Ok(_) => panic!("expected the stuck body to time out"),
        }
    }

    /// Body that ends cleanly. The wrapper must pass `Ready(None)`
    /// straight through and never fire the timer.
    #[tokio::test(flavor = "current_thread")]
    async fn ends_cleanly_on_eos() {
        use http_body_util::{BodyExt, Full};
        let body = TimedBody::new(
            Full::<Bytes>::new(Bytes::from_static(b"payload")),
            Duration::from_secs(60),
        );
        let collected = body.collect().await.expect("Full never errors");
        assert_eq!(&collected.to_bytes()[..], b"payload");
    }
}
