//! Phase 7 acceptance: end-to-end H2 round-trips through a real
//! loopback socket, using hyper's http2 client in "prior knowledge"
//! mode (h2c — no TLS, no ALPN). The full TLS+ALPN path is exercised
//! in the binary, not here; this crate's job is to prove the server
//! half drives a real H2 connection to completion.

use bytes::Bytes;
use conduit_proto::{Response, StatusCode, VecBody};
use http::Request;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_get_round_trip() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server = async {
        let (stream, _peer) = listener.accept().await.expect("accept");
        conduit_h2::serve_connection(stream, |_req: Request<Incoming>| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(VecBody::from_bytes(Bytes::from_static(
                        b"hello from conduit-h2",
                    )))
                    .expect("build response"),
            )
        })
        .await
        .expect("serve");
    };

    let client = async {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Empty<Bytes>>(
            TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await
        .expect("h2 handshake");
        // Drive the client connection in the background. It returns
        // Err once the server closes its side; that's expected.
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .uri("http://x/hello")
            .body(Empty::<Bytes>::new())
            .expect("build req");
        let resp = sender.send_request(req).await.expect("send");
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes();
        (status, body)
    };

    let ((), (status, body)) = tokio::join!(server, client);
    assert_eq!(status, 200);
    assert_eq!(&body[..], b"hello from conduit-h2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_post_round_trip_with_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let payload = b"the quick brown fox";

    let server = async {
        let (stream, _peer) = listener.accept().await.expect("accept");
        conduit_h2::serve_connection(stream, |req: Request<Incoming>| async move {
            let body = req.into_body().collect().await.expect("collect").to_bytes();
            let msg = format!("got {} bytes", body.len());
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(VecBody::from_bytes(Bytes::from(msg)))
                    .expect("build response"),
            )
        })
        .await
        .expect("serve");
    };

    let client = async {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Full<Bytes>>(
            TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await
        .expect("h2 handshake");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method("POST")
            .uri("http://x/echo")
            .body(Full::new(Bytes::from_static(payload)))
            .expect("build req");
        let resp = sender.send_request(req).await.expect("send");
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes();
        (status, body)
    };

    let ((), (status, body)) = tokio::join!(server, client);
    assert_eq!(status, 200);
    assert_eq!(&body[..], format!("got {} bytes", payload.len()).as_bytes());
}
