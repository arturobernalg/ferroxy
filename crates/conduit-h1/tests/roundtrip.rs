//! Phase 3 acceptance: end-to-end H1 round-trips through a real
//! loopback socket.
//!
//! Tests cover:
//!   - GET → 200 OK with full body returned
//!   - POST with body → handler reads body, echoes length back
//!   - keep-alive: two requests over one connection
//!
//! Pipelining (explicit), chunked-trailers, malformed-input rejection,
//! and the cargo-fuzz target are deferred to P3.x — see
//! `phase3_deviations` in project memory.
//!
//! Each test runs server and client side-by-side via `tokio::join!`
//! rather than `tokio::spawn`. The spawn-based pattern raced under
//! the multi-thread test harness (the spawn could be still pending
//! when the client connect-and-write completed). join! makes the
//! ordering explicit.

use std::net::SocketAddr;

use bytes::Bytes;
use conduit_proto::{Response, StatusCode, VecBody};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_until_close(mut s: TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    buf
}

/// Bind a fresh loopback listener and return it together with its
/// resolved address.
async fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_round_trip() {
    let (listener, addr) = loopback().await;

    let server = async {
        let (stream, _peer) = listener.accept().await.expect("accept");
        conduit_h1::serve_connection(stream, |_req: http::Request<Incoming>| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(VecBody::from_bytes(Bytes::from_static(
                        b"hello from conduit-h1",
                    )))
                    .expect("build response"),
            )
        })
        .await
        .expect("serve");
    };

    let client = async {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        s.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        // Do not shut down the client write half. With
        // `Connection: close` set, the server closes after responding,
        // which ends `read_to_end` deterministically. Half-closing the
        // client side caused hyper to surface `IncompleteMessage` on
        // some test runs (likely a peer-EOF-mid-pipeline interaction
        // in the keep-alive read loop); not shutting down sidesteps
        // it without affecting wire semantics — the server-driven
        // close still cleanly ends the test.
        read_until_close(s).await
    };

    let ((), bytes) = tokio::join!(server, client);
    let response = String::from_utf8(bytes).expect("utf8");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response:?}");
    assert!(
        response.contains("content-type: text/plain"),
        "got: {response:?}"
    );
    assert!(
        response.ends_with("hello from conduit-h1"),
        "got: {response:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_round_trip_with_body() {
    let (listener, addr) = loopback().await;
    let payload = b"the quick brown fox";

    let server = async {
        let (stream, _peer) = listener.accept().await.expect("accept");
        conduit_h1::serve_connection(stream, |req: http::Request<Incoming>| async move {
            let body = req.into_body().collect().await.expect("collect").to_bytes();
            let response = format!("got {} bytes", body.len());
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(VecBody::from_bytes(Bytes::from(response)))
                    .expect("build response"),
            )
        })
        .await
        .expect("serve");
    };

    let client = async {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let req = format!(
            "POST /echo HTTP/1.1\r\n\
             Host: x\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            payload.len()
        );
        s.write_all(req.as_bytes()).await.expect("write head");
        s.write_all(payload).await.expect("write body");
        // Do not shut down the client write half. With
        // `Connection: close` set, the server closes after responding,
        // which ends `read_to_end` deterministically. Half-closing the
        // client side caused hyper to surface `IncompleteMessage` on
        // some test runs (likely a peer-EOF-mid-pipeline interaction
        // in the keep-alive read loop); not shutting down sidesteps
        // it without affecting wire semantics — the server-driven
        // close still cleanly ends the test.
        read_until_close(s).await
    };

    let ((), bytes) = tokio::join!(server, client);
    let response = String::from_utf8(bytes).expect("utf8");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response:?}");
    assert!(
        response.ends_with(&format!("got {} bytes", payload.len())),
        "got: {response:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_alive_two_requests() {
    let (listener, addr) = loopback().await;

    let server = async {
        let (stream, _peer) = listener.accept().await.expect("accept");
        conduit_h1::serve_connection(stream, |_req: http::Request<Incoming>| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(VecBody::from_bytes(Bytes::from_static(b"pong")))
                    .expect("build response"),
            )
        })
        .await
        .expect("serve");
    };

    let client = async {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        // Two requests back-to-back. The second sets Connection: close
        // so the server closes after responding, letting read_to_end
        // complete deterministically.
        s.write_all(
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n\
              GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write");
        // Do not shut down the client write half. With
        // `Connection: close` set, the server closes after responding,
        // which ends `read_to_end` deterministically. Half-closing the
        // client side caused hyper to surface `IncompleteMessage` on
        // some test runs (likely a peer-EOF-mid-pipeline interaction
        // in the keep-alive read loop); not shutting down sidesteps
        // it without affecting wire semantics — the server-driven
        // close still cleanly ends the test.
        read_until_close(s).await
    };

    let ((), bytes) = tokio::join!(server, client);
    let response = String::from_utf8(bytes).expect("utf8");

    let pong_count = response.matches("pong").count();
    assert_eq!(pong_count, 2, "expected two responses, got: {response:?}");
}
