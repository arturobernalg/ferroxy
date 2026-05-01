//! conduit-h1 — HTTP/1.1 (RFC 9110 + 9112) ingress over an arbitrary
//! tokio-shaped byte stream.
//!
//! # What this crate does
//!
//! [`serve_connection`] takes one already-connected stream and a
//! per-request handler closure, drives an HTTP/1.1 server connection
//! to completion (one request, or many under keep-alive), and returns
//! when the client closes or any side errors.
//!
//! Internally it composes hyper's `http1::Builder::serve_connection`
//! against a `service_fn(handler)`. Every public type the caller
//! touches is `http::Request<hyper::body::Incoming>` /
//! `http::Response<B>`; nothing of hyper leaks into the type signature.
//!
//! # What this crate does not do (yet)
//!
//! - **Per-request bumpalo arena** for header storage. Charter rule.
//!   Deferred to a P3.x cleanup pass; surfaced in
//!   `phase3_deviations` in project memory.
//! - **Pipelining** explicit acceptance test. hyper handles pipelining
//!   correctly under the hood; the explicit test against malformed
//!   pipelined input lands in P3.x alongside the fuzz target.
//! - **Malformed input fuzzer** via `cargo-fuzz`. Required by the
//!   quality gate for parsers; deferred with the rest of P3.x.
//! - **Wire integration with conduit-io's monoio stream**. P3 takes a
//!   tokio stream; the monoio→tokio bridge using `monoio-compat` lands
//!   when conduit-upstream (P4) needs the same bridge for client
//!   connections, so it can be done once.

#![deny(missing_docs)]

use std::convert::Infallible;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

use conduit_proto::{Body, Request, Response};

/// Errors surfaced from [`serve_connection`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// hyper reported a connection-level error: malformed request
    /// head, framing violation, peer closed mid-frame, etc.
    #[error("hyper connection error")]
    Hyper(#[from] hyper::Error),
}

/// Drive one HTTP/1.1 server connection to completion.
///
/// `io` is a tokio-shaped byte stream (typically a `TcpStream`, or a
/// TLS-wrapped one in later phases). `handler` is invoked once per
/// incoming request; the returned response body is streamed back to
/// the client.
///
/// Keep-alive is enabled by default in hyper's HTTP/1 server; the
/// connection survives across requests until the peer half-closes or
/// either side hits an error.
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
    hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await?;
    Ok(())
}
