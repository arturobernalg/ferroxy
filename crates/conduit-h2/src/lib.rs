//! conduit-h2 — HTTP/2 (RFC 9113) ingress over an arbitrary
//! tokio-shaped byte stream (typically a TLS-wrapped TCP stream
//! whose ALPN negotiated `h2`).
//!
//! # What this crate does
//!
//! [`serve_connection`] takes one already-connected stream and a
//! per-request handler closure, drives an HTTP/2 server connection
//! to completion (multiplexed streams over one connection), and
//! returns when the peer closes or any side errors.
//!
//! Internally it composes hyper's `http2::Builder::serve_connection`
//! against a `service_fn(handler)` over a `TokioExecutor`. The public
//! shape mirrors `conduit-h1::serve_connection` exactly so callers can
//! pick a serve function based on the ALPN-negotiated protocol without
//! duplicating their service wiring.
//!
//! # What this crate does not do (yet)
//!
//! - **HPACK fuzz target** via `cargo-fuzz`. Required by the charter
//!   for parser hot paths; deferred to P7.x. Hyper itself uses the
//!   `h2` crate which has its own HPACK fuzz coverage upstream, so
//!   we are not currently exposed.
//! - **`h2spec` 100% pass** as a CI gate. Charter rule. The first run
//!   against an MVP server typically fails on the per-stream
//!   flow-control settings and SETTINGS-ack timing; tracked in
//!   `phase7_deviations`.
//! - **Server-pushed responses** (deprecated by every browser; we will
//!   not implement push).
//! - **Adaptive flow-control window tuning**. Defaults to hyper's
//!   `initial_stream_window_size = 64 KiB`, `initial_connection_window_size`
//!   `= 1 MiB`; tuned in P11 once the bench harness shows whether the
//!   defaults bottleneck on our 25 Gbps target.

#![deny(missing_docs)]

use std::convert::Infallible;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite};

use conduit_proto::{Body, Request, Response};

/// Errors surfaced from [`serve_connection`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// hyper reported a connection-level error: malformed frame,
    /// HPACK error, GOAWAY received, peer closed mid-stream, etc.
    #[error("hyper h2 connection error")]
    Hyper(#[from] hyper::Error),
}

/// Drive one HTTP/2 server connection to completion.
///
/// `io` is a tokio-shaped byte stream — for production HTTP/2 this is
/// a `tokio_rustls::server::TlsStream<TcpStream>` whose ALPN negotiated
/// `h2`. `handler` is invoked once per incoming request stream; the
/// returned response body is streamed back to the client over that
/// stream's frames.
///
/// HTTP/2 multiplexes many request streams over one connection. The
/// connection lives until either side sends GOAWAY or the underlying
/// stream errors.
pub async fn serve_connection<IO, F, Fut, B>(io: IO, handler: F) -> Result<(), ServeError>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = Result<Response<B>, Infallible>> + Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let io = TokioIo::new(io);
    let service = service_fn(move |req| {
        let h = handler.clone();
        async move { h(req).await }
    });
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await?;
    Ok(())
}
