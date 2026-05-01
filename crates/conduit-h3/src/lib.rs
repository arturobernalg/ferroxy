//! conduit-h3 — HTTP/3 (RFC 9114) ingress over QUIC.
//!
//! # What this crate does
//!
//! [`serve_connection`] takes one already-established `quinn::Connection`
//! (post-QUIC-handshake, ALPN-negotiated `h3`) and a per-request handler
//! closure, drives an HTTP/3 server connection to completion (multiplexed
//! request streams), and returns when the peer closes or any side errors.
//!
//! Internally it wraps the connection with `h3-quinn::Connection` and
//! drives `h3::server::Connection::accept` in a loop, spawning a tokio
//! task per request stream. The handler signature mirrors the H1 / H2
//! crates as closely as the protocol allows: it takes a
//! `http::Request<Bytes>` (the request body is buffered before the
//! handler runs, matching the P4 buffered-body assumption) and returns
//! `http::Response<conduit_proto::BoxBody>`.
//!
//! # What this crate does not do (yet)
//!
//! - **Streaming request bodies**. The H3 stream is drained into a
//!   single `Bytes` before the handler is invoked. Charter target
//!   workload has <1 KB request bodies p95; the buffering cost is
//!   negligible. Wiring a streaming `http_body::Body` impl over h3's
//!   `recv_data` is P9.x scope.
//! - **0-RTT**. Off by default per charter, no toggle yet.
//! - **QUIC interop testing** (qlog capture, h3spec). Charter rule;
//!   surfaced in `phase9_deviations` in project memory.
//! - **HTTP/3 client** (egress over h3). Reverse proxies overwhelmingly
//!   speak h1/h2 to backends; an h3 upstream is P9.x scope.

#![deny(missing_docs)]

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response};

use conduit_proto::Body;

/// Errors surfaced from [`serve_connection`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// h3 reported a connection-level error (HPACK / framing /
    /// peer-closed-mid-stream).
    #[error("h3 connection error: {0}")]
    H3Connection(String),
}

/// Drive one HTTP/3 server connection to completion.
///
/// `conn` is the already-established QUIC connection. `handler` is
/// invoked once per incoming request stream; the request body is fully
/// buffered into `Bytes` before the handler runs, matching the P4
/// target workload (<1 KB request bodies p95).
pub async fn serve_connection<F, Fut, B>(
    conn: quinn::Connection,
    handler: F,
) -> Result<(), ServeError>
where
    F: Fn(Request<Bytes>) -> Fut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = Result<Response<B>, Infallible>> + Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let h3_conn = h3_quinn::Connection::new(conn);
    let mut h3 = h3::server::Connection::<_, Bytes>::new(h3_conn)
        .await
        .map_err(|e| ServeError::H3Connection(e.to_string()))?;

    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(e) = drive_request(resolver, &handler).await {
                        tracing::debug!(error = %e, "h3 request stream ended with error");
                    }
                });
            }
            Ok(None) => break, // peer closed cleanly
            Err(e) => {
                return Err(ServeError::H3Connection(e.to_string()));
            }
        }
    }
    Ok(())
}

/// Drive one accepted h3 request stream: collect the request body,
/// invoke the handler, stream the response back over `send_data` and
/// finish the stream. Errors surface to the caller for logging.
async fn drive_request<F, Fut, B>(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    handler: &F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(Request<Bytes>) -> Fut,
    Fut: std::future::Future<Output = Result<Response<B>, Infallible>>,
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let (req_head, mut body_stream) = resolver.resolve_request().await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(IoStr(e.to_string())) },
    )?;
    let (req_parts, ()) = req_head.into_parts();
    let mut buf = Vec::new();
    while let Some(mut chunk) =
        body_stream
            .recv_data()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(IoStr(e.to_string()))
            })?
    {
        // h3's `Buf` may not be a single contiguous slice; drain it.
        while bytes::Buf::has_remaining(&chunk) {
            let chunk_bytes = bytes::Buf::chunk(&chunk);
            buf.extend_from_slice(chunk_bytes);
            let n = chunk_bytes.len();
            bytes::Buf::advance(&mut chunk, n);
        }
    }
    let req = Request::from_parts(req_parts, Bytes::from(buf));

    let resp = handler(req).await.unwrap_or_else(|never| match never {});
    let (resp_parts, resp_body) = resp.into_parts();
    let head: Response<()> = Response::from_parts(resp_parts, ());
    body_stream.send_response(head).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(IoStr(e.to_string())) },
    )?;

    // Stream the response body. `Body::frame` yields one frame at a
    // time; we forward data frames as `send_data` and drop non-data
    // frames (trailers don't appear in conduit's response bodies
    // today — our boxes are built from VecBody / hyper Incoming +
    // TimedBody wrappers, none of which emit trailers).
    //
    // Stack-pin the body so we don't require `B: Unpin`. Caller
    // bodies that contain a !Unpin field (e.g. `TimedBody`'s inner
    // `Sleep`) work without an extra `Box::pin` allocation.
    let mut resp_body = std::pin::pin!(resp_body);
    while let Some(frame) = std::future::poll_fn(|cx| resp_body.as_mut().poll_frame(cx)).await {
        let frame = frame.map_err(Into::into)?;
        if let Ok(data) = frame.into_data() {
            body_stream.send_data(data).await.map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(IoStr(e.to_string())) },
            )?;
        }
    }
    body_stream
        .finish()
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(IoStr(e.to_string()))
        })?;
    Ok(())
}

/// Tiny `Error`-shaped wrapper for h3's stringly-typed errors so we
/// can surface them in the trait-object chain without pulling in a
/// dependency on `anyhow`.
#[derive(Debug)]
struct IoStr(String);
impl std::fmt::Display for IoStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for IoStr {}

/// Build a `quinn::ServerConfig` from a `rustls::ServerConfig` ready
/// for QUIC. The supplied rustls config must already have `h3` in its
/// `alpn_protocols`. Helper exists so the binary can keep the cert
/// loading concentrated in conduit-transport.
///
/// QUIC requires TLS 1.3; rustls 0.23 ships with a TLS-1.3-capable
/// `ring` provider by default, which matches the rest of the conduit
/// stack (see `phase6_deviations`).
pub fn quic_server_config(
    rustls_cfg: rustls::ServerConfig,
) -> Result<quinn::ServerConfig, ServeError> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
        .map_err(|e| ServeError::H3Connection(format!("rustls→quic: {e}")))?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}
