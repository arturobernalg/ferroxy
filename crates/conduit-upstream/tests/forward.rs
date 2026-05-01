//! Phase 4 acceptance: upstream forwarding with retry.
//!
//! Tests against a tiny in-process mock upstream:
//!   - happy path: GET → 200 OK with body
//!   - retry on 502: first attempt 502, second 200, retry succeeds
//!   - retry exhausted: every attempt returns 502; the test asserts
//!     the final upstream status flows back to the caller
//!
//! Each test runs the mock upstream and the client side-by-side via
//! `tokio::join!` (same pattern as `conduit-h1`'s tests; see
//! `phase3_deviations` for why we do not half-close the client write).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use conduit_upstream::{RetryPolicy, Upstream};
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Bind a fresh loopback listener.
async fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

/// Drive a single mock-upstream connection, returning the request
/// bytes seen on the wire so tests can assert what hyper-util sent.
async fn mock_upstream_capture(stream: TcpStream, response_bytes: &[u8]) -> Vec<u8> {
    let mut s = stream;
    let mut buf = [0u8; 4096];
    let mut accum = Vec::with_capacity(512);
    while !accum.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => accum.extend_from_slice(&buf[..n]),
            Err(_) => return accum,
        }
    }
    let _ = s.write_all(response_bytes).await;
    let _ = s.shutdown().await;
    accum
}

/// Drive a single mock-upstream connection: read the request line +
/// headers, then write `response_bytes` and close. The mock
/// intentionally does no body parsing — the test controls exactly
/// what bytes go on the wire.
async fn mock_upstream_response(stream: TcpStream, response_bytes: &[u8]) {
    let mut s = stream;
    let mut buf = [0u8; 4096];
    // Read until end-of-headers (`\r\n\r\n`).
    let mut accum = Vec::with_capacity(512);
    while !accum.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => accum.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    }
    // Consume any Content-Length body if present (small; we just
    // discard).
    if let Some(cl) = accum
        .windows(b"content-length:".len())
        .position(|w| w.eq_ignore_ascii_case(b"content-length:"))
    {
        // Find the digits after "content-length:" up to CRLF.
        let after = &accum[cl + b"content-length:".len()..];
        let end = after
            .iter()
            .position(|&b| b == b'\r')
            .unwrap_or(after.len());
        let cl_str = std::str::from_utf8(&after[..end]).unwrap_or("0").trim();
        let cl_val: usize = cl_str.parse().unwrap_or(0);
        let header_end = accum
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(accum.len(), |i| i + 4);
        let body_seen_already = accum.len().saturating_sub(header_end);
        let still_to_read = cl_val.saturating_sub(body_seen_already);
        if still_to_read > 0 {
            let mut left = still_to_read;
            while left > 0 {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                left = left.saturating_sub(n);
            }
        }
    }
    // Write response and close.
    let _ = s.write_all(response_bytes).await;
    let _ = s.shutdown().await;
}

/// Build a minimal HTTP/1.1 response.
fn http_response(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        reason,
        body.len(),
        body
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_happy_path() {
    let (listener, addr) = loopback().await;

    let upstream = Upstream::new(RetryPolicy::none());
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://{addr}/"))
        .header("host", addr.to_string())
        .body(Full::new(Bytes::new()))
        .expect("build");

    let server = async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        mock_upstream_response(stream, &http_response(200, "OK", "hello upstream")).await;
    };

    let client = async { upstream.forward(req).await.expect("forward") };

    let ((), resp) = tokio::join!(server, client);
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    assert_eq!(&body[..], b"hello upstream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_retries_on_502_then_succeeds() {
    let (listener, addr) = loopback().await;

    let upstream = Upstream::new(RetryPolicy {
        attempts: 3,
        on_status: vec![502],
        on_connect_error: true,
    });

    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://{addr}/"))
        .header("host", addr.to_string())
        .body(Full::new(Bytes::new()))
        .expect("build");

    // Server side: first attempt 502, second 200.
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = Arc::clone(&calls);
    let server = async move {
        for _ in 0..2 {
            let (stream, _peer) = listener.accept().await.expect("accept");
            let n = server_calls.fetch_add(1, Ordering::SeqCst);
            let response = if n == 0 {
                http_response(502, "Bad Gateway", "first try fails")
            } else {
                http_response(200, "OK", "second try wins")
            };
            mock_upstream_response(stream, &response).await;
        }
    };

    let client = async { upstream.forward(req).await };

    let ((), result) = tokio::join!(server, client);
    let resp = result.expect("forward");
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    assert_eq!(&body[..], b"second try wins");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_retries_exhausted() {
    let (listener, addr) = loopback().await;

    let upstream = Upstream::new(RetryPolicy {
        attempts: 2,
        on_status: vec![502],
        on_connect_error: false,
    });

    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://{addr}/"))
        .header("host", addr.to_string())
        .body(Full::new(Bytes::new()))
        .expect("build");

    // Server returns 502 every time.
    let server = async move {
        for _ in 0..2 {
            let (stream, _peer) = listener.accept().await.expect("accept");
            mock_upstream_response(stream, &http_response(502, "Bad Gateway", "no")).await;
        }
    };

    let client = async { upstream.forward(req).await };

    let ((), result) = tokio::join!(server, client);
    // Design choice: when retries are exhausted on a retry-eligible
    // status, `forward` returns the final upstream response (not a
    // synthesised error). Callers see the actual upstream headers
    // and body and can apply per-route policy. The
    // `ForwardError::RetriesExhausted` variant is reserved for the
    // future case where retries exhaust without any usable response
    // (e.g. all attempts hit connect errors).
    let resp = result.expect("forward should yield the last response on exhausted retries");
    assert_eq!(resp.status(), http::StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_ingress_translates_to_h1_egress() {
    // Phase 8: an inbound Request whose version is HTTP/2.0 must hit
    // the upstream as HTTP/1.1 over the wire. Charter calls this the
    // production reverse-proxy path: clients speak h2, backends speak
    // h1, conduit translates between them.
    let (listener, addr) = loopback().await;

    let upstream = Upstream::new(RetryPolicy::none());
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://{addr}/h2-path"))
        .version(http::Version::HTTP_2)
        // Pseudo-headers (`:authority`, `:scheme`, …) are already
        // surfaced as Request parts by hyper's H2 server — no need to
        // pass them as headers here. We do leave a normal `host` to
        // verify upstream strips it before reissuing under the
        // rewritten URI.
        .header("host", "client-was-here.example")
        .body(Full::new(Bytes::new()))
        .expect("build");

    let server = async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        mock_upstream_capture(stream, &http_response(200, "OK", "ok")).await
    };
    let client = async { upstream.forward(req).await.expect("forward") };

    let (request_bytes, resp) = tokio::join!(server, client);
    assert_eq!(resp.status(), http::StatusCode::OK);

    let request_text = String::from_utf8_lossy(&request_bytes);
    assert!(
        request_text.contains("HTTP/1.1"),
        "expected HTTP/1.1 on wire, got: {request_text:?}",
    );
    // The inbound `host: client-was-here.example` should NOT have
    // reached the backend — hyper-util re-adds Host from the
    // rewritten URI authority (the backend addr).
    assert!(
        !request_text.contains("client-was-here.example"),
        "stale Host leaked through: {request_text:?}",
    );
    assert!(
        request_text.contains("/h2-path"),
        "request line lost path: {request_text:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breaker_opens_after_consecutive_connect_failures() {
    use conduit_upstream::{BreakerConfig, ForwardError};

    // Bind and immediately drop the listener so the address points at
    // nothing — every connection attempt will fail.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let dead_addr = listener.local_addr().expect("local_addr");
    drop(listener);

    let upstream = Upstream::with_breaker(
        RetryPolicy::none(),
        vec![dead_addr],
        BreakerConfig {
            threshold: 2,
            cooldown: std::time::Duration::from_secs(60),
        },
    );

    // Helper: build a fresh request for each attempt (Request<Full>
    // is single-use — the body buffer is consumed by forward()).
    let req = || {
        http::Request::builder()
            .uri(format!("http://{dead_addr}/"))
            .body(Full::new(Bytes::new()))
            .expect("build")
    };

    // First attempt: real connect failure.
    match upstream.forward(req()).await {
        Err(ForwardError::Client { connect_error, .. }) => assert!(connect_error),
        other => panic!("expected Client connect-error, got {other:?}"),
    }
    // Second attempt: trips the breaker (now at threshold).
    match upstream.forward(req()).await {
        Err(ForwardError::Client { connect_error, .. }) => assert!(connect_error),
        other => panic!("expected Client connect-error, got {other:?}"),
    }
    // Third attempt: breaker is open, no connection attempted.
    match upstream.forward(req()).await {
        Err(ForwardError::BreakerOpen) => {}
        other => panic!("expected BreakerOpen, got {other:?}"),
    }
}
