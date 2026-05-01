//! Phase 5 acceptance: end-to-end dispatch through a route table.
//!
//! Two integration tests exercise the full match-and-forward path:
//!   - dispatch routes to the matching upstream
//!   - dispatch returns `NoRoute` when nothing matches
//!
//! The Upstream registered here is built directly with
//! `Upstream::new(RetryPolicy::none())` so the test does not depend
//! on conduit-config; that side of the bridge is exercised by the
//! `*_from_config` constructors' unit tests in lib + table.

use std::net::SocketAddr;

use bytes::Bytes;
use conduit_lifecycle::{Dispatch, DispatchError, PathMatch, RouteTable, UpstreamMap};
use conduit_upstream::{RetryPolicy, Upstream};
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

async fn mock_response(stream: TcpStream, response: &[u8]) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_routes_to_matching_upstream() {
    let (listener, addr) = loopback().await;

    let mut upstreams = UpstreamMap::new();
    upstreams.insert("api", Upstream::new(RetryPolicy::none()));

    // Synthesise a Route by hand instead of going through Config.
    let routes = RouteTable::from_config(&dummy_config_with_route(
        Some("api.example.com"),
        &PathMatch::Prefix("/".into()),
        "api",
    ));
    let dispatch = Dispatch::new(routes, upstreams);

    // Build a request whose URI host points to the mock upstream's
    // address, and whose Host header matches the route. The upstream
    // client follows the URI; the routing matches on Host.
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://{addr}/"))
        .header("host", "api.example.com")
        .body(Full::new(Bytes::new()))
        .expect("build request");

    let server = async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        mock_response(stream, &http_response(200, "OK", "routed")).await;
    };

    let client = async { dispatch.handle(req).await };

    let ((), result) = tokio::join!(server, client);
    let resp = result.expect("dispatch should succeed");
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    assert_eq!(&body[..], b"routed");
}

#[tokio::test]
async fn dispatch_returns_no_route_when_nothing_matches() {
    let upstreams = UpstreamMap::new();
    let routes = RouteTable::from_config(&dummy_config_with_route(
        Some("api.example.com"),
        &PathMatch::Any,
        "api",
    ));
    let dispatch = Dispatch::new(routes, upstreams);

    // Wrong host — should not match the only route.
    let req = http::Request::builder()
        .uri("http://other.example.com/")
        .header("host", "other.example.com")
        .body(Full::new(Bytes::new()))
        .expect("build");

    match dispatch.handle(req).await {
        Err(DispatchError::NoRoute) => {}
        other => panic!("expected NoRoute, got {other:?}"),
    }
}

/// Construct a one-route conduit-config Config in-process without
/// going through TOML. Lets the test drive the lifecycle layer
/// without bringing in tempfile / TOML literals.
fn dummy_config_with_route(
    host: Option<&str>,
    path: &PathMatch,
    upstream: &str,
) -> conduit_config::Config {
    let mut fields: Vec<String> = Vec::new();
    if let Some(h) = host {
        fields.push(format!("host = \"{h}\""));
    }
    match path {
        PathMatch::Any => {}
        PathMatch::Prefix(p) => fields.push(format!("path_prefix = \"{p}\"")),
        PathMatch::Exact(p) => fields.push(format!("path_exact = \"{p}\"")),
    }
    // Validator rejects empty match; we always pass at least one
    // field. PathMatch::Any with no host is the only edge-case the
    // tests above don't hit, so we don't have to special-case it.
    let match_block = fields.join(", ");
    let toml_text = format!(
        r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "{upstream}"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = {{ {match_block} }}
upstream = "{upstream}"
"#,
    );
    conduit_config::parse(&toml_text).expect("parse synthetic config")
}
