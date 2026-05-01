//! End-to-end integration: real HTTP/1.1 client → `conduit-h1` ingress
//! → `conduit-lifecycle::Dispatch` → `conduit-upstream` → mock upstream.
//!
//! This is the first test that drives the full library composition
//! the binary will eventually run. It demonstrates that the contracts
//! between phase 3 (h1), phase 4 (upstream), and phase 5 (lifecycle)
//! compose without glue.
//!
//! The "binary side" of this composition (using monoio runtime via
//! `conduit-io`) waits on a monoio↔tokio bridge, which is the next
//! deferred follow-up. This test runs entirely on tokio.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use conduit_lifecycle::{Dispatch, RouteTable, UpstreamMap};
use conduit_proto::{Response, StatusCode};
use conduit_upstream::{RetryPolicy, Upstream};
use http_body_util::Full;
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Bind a fresh loopback listener.
async fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

/// Drive a single mock-upstream connection: read until end of headers,
/// then write `response` and close.
async fn mock_upstream(stream: TcpStream, response: &[u8]) {
    let mut s = stream;
    let mut buf = [0u8; 4096];
    let mut accum = Vec::with_capacity(512);
    while !accum.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => accum.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    }
    let _ = s.write_all(response).await;
    let _ = s.shutdown().await;
}

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

fn dispatch_with_route_to(addr: SocketAddr) -> Dispatch {
    // Build a one-route lifecycle that maps the test virtual host to
    // the upstream named "u". The upstream is NOT taken from config
    // (would need address parsing); it's built directly so we can
    // control retry policy.
    let mut upstreams = UpstreamMap::new();
    upstreams.insert("u", Upstream::new(RetryPolicy::none()));
    let _ = addr; // upstream URL is supplied via per-request URI in
                  // the proxy handler below — this test exercises the
                  // routing decision, not the upstream config wiring
                  // (which is unit-tested separately in
                  // conduit-config + conduit-upstream).

    let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "u"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "front.example.com", path_prefix = "/" }
upstream = "u"
"#;
    let cfg = conduit_config::parse(toml).expect("parse");
    let routes = RouteTable::from_config(&cfg);
    Dispatch::new(routes, upstreams)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_ingress_to_upstream_round_trip() {
    // Three actors:
    //   1. mock upstream (TcpListener) — replies with a fixed body
    //   2. conduit ingress (TcpListener) — runs conduit-h1, dispatches
    //      to lifecycle, lifecycle forwards to mock upstream
    //   3. test client — connects to ingress, writes a real H1 GET
    //
    // The proxy handler in (2) rewrites the request's URI to point at
    // the mock upstream's address — the real binary will derive this
    // from the route's `[[upstream]].addrs` list (load balancing,
    // health, etc.). This test stops at "lifecycle decides which
    // upstream to forward to"; choosing an address from that
    // upstream's addrs list is per-load-balancer logic that lands
    // alongside the LB algorithm work in P5.x.

    let (upstream_listener, upstream_addr) = loopback().await;
    let (ingress_listener, ingress_addr) = loopback().await;

    let upstream_response = http_response(200, "OK", "from-mock-upstream");

    let upstream_server = async move {
        let (stream, _) = upstream_listener.accept().await.expect("upstream accept");
        mock_upstream(stream, &upstream_response).await;
    };

    let dispatch = Arc::new(dispatch_with_route_to(upstream_addr));

    let proxy = async move {
        let (stream, _) = ingress_listener.accept().await.expect("ingress accept");
        let dispatch_for_handler = Arc::clone(&dispatch);
        let handler = move |mut req: http::Request<Incoming>| {
            let dispatch = Arc::clone(&dispatch_for_handler);
            async move {
                // Rewrite the request's URI to the mock upstream's
                // address. Real LB selection lives in P5.x; this test
                // just proves dispatch + upstream forward compose.
                let new_uri: http::Uri = format!("http://{upstream_addr}{}", req.uri().path())
                    .parse()
                    .expect("uri parse");
                *req.uri_mut() = new_uri;
                let resp = match dispatch.handle(req).await {
                    Ok(r) => {
                        // Convert hyper::body::Incoming → BoxBody for
                        // serialisation. The map_err coerces hyper's
                        // body error into the boxed error type.
                        let (parts, body) = r.into_parts();
                        let body: conduit_proto::BoxBody = http_body_util::BodyExt::boxed(
                            http_body_util::BodyExt::map_err(body, |e| {
                                let b: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
                                b
                            }),
                        );
                        Ok::<_, Infallible>(Response::from_parts(parts, body))
                    }
                    Err(e) => {
                        let body = conduit_proto::boxed(conduit_proto::VecBody::from_bytes(
                            Bytes::from(format!("dispatch error: {e:?}")),
                        ));
                        let resp = Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(body)
                            .expect("error response");
                        Ok::<_, Infallible>(resp)
                    }
                };
                resp
            }
        };
        if let Err(e) = conduit_h1::serve_connection(stream, handler).await {
            eprintln!("ingress serve error: {e:?}");
        }
    };

    let client = async {
        let mut s = TcpStream::connect(ingress_addr).await.expect("connect");
        s.write_all(
            b"GET /api/items HTTP/1.1\r\n\
              Host: front.example.com\r\n\
              Connection: close\r\n\
              \r\n",
        )
        .await
        .expect("write");
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        buf
    };

    let ((), (), client_bytes) = tokio::join!(upstream_server, proxy, client);
    let response = String::from_utf8(client_bytes).expect("utf8");

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected 200 OK, got: {response:?}"
    );
    assert!(
        response.ends_with("from-mock-upstream"),
        "expected mock body, got: {response:?}"
    );
}

/// Helper: bound by `Full<Bytes>` so the route is exercised with a
/// concrete body type at compile time. (The end-to-end test uses
/// `Incoming` from hyper's `serve_connection`.)
fn _typecheck_request_with_full_body() {
    let _: http::Request<Full<Bytes>> = http::Request::builder()
        .uri("http://x/")
        .body(Full::new(Bytes::new()))
        .unwrap();
}
