//! Contract tests for the request/response stream type.
//!
//! These tests assert the **invariants** every protocol implementation
//! and every lifecycle consumer can rely on, regardless of which
//! protocol crate produced the request:
//!
//! - request and response heads round-trip through `http`'s builders
//!   without losing data,
//! - bodies generic over `B: BodyBytes` compose with the head types,
//! - cancellation tokens propagate from one clone to another,
//! - trailers, when present, follow data frames in `Body::poll_frame`
//!   ordering.
//!
//! No protocol code is exercised here. The point is to nail down the
//! shape of the contract so phases 3–9 cannot accidentally diverge.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use bytes::Bytes;
use conduit_proto::{
    boxed, Body, BodyBytes, Cancellation, CancellationError, ErasedRequest, ErasedResponse,
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, VecBody,
};
use http_body_util::BodyExt;

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    struct Nop;
    impl Wake for Nop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Nop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

#[test]
fn request_head_round_trip() {
    let body = VecBody::from_bytes(Bytes::from_static(b"hello"));
    let req: Request<VecBody> = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/api/v1/items?id=42")
        .header("content-type", "application/json")
        .body(body)
        .expect("builder");

    assert_eq!(req.method(), Method::POST);
    assert_eq!(req.uri().path(), "/api/v1/items");
    assert_eq!(req.uri().query(), Some("id=42"));
    assert_eq!(
        req.headers().get("content-type").map(HeaderValue::as_bytes),
        Some(&b"application/json"[..])
    );

    let collected = block_on(req.into_body().collect()).expect("Infallible");
    assert_eq!(&collected.to_bytes()[..], b"hello");
}

#[test]
fn response_head_round_trip() {
    let body = VecBody::from_bytes(Bytes::from_static(b"ok"));
    let resp: Response<VecBody> = Response::builder()
        .status(StatusCode::OK)
        .header("x-conduit", "yes")
        .body(body)
        .expect("builder");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-conduit").map(HeaderValue::as_bytes),
        Some(&b"yes"[..])
    );

    let collected = block_on(resp.into_body().collect()).expect("Infallible");
    assert_eq!(&collected.to_bytes()[..], b"ok");
}

#[test]
fn body_bytes_bound_is_satisfied_by_vec_body() {
    fn require<B: BodyBytes<Error = std::convert::Infallible>>(_: B) {}
    require(VecBody::empty());
}

#[test]
fn boxed_body_erases_concrete_type() {
    let body = VecBody::from_chunks([Bytes::from_static(b"a"), Bytes::from_static(b"bcd")]);
    let req: ErasedRequest = Request::builder()
        .uri("/")
        .body(boxed(body))
        .expect("builder");
    let resp: ErasedResponse = Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(boxed(VecBody::empty()))
        .expect("builder");

    assert_eq!(req.uri(), &Uri::from_static("/"));
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let body = block_on(req.into_body().collect()).expect("collect");
    assert_eq!(&body.to_bytes()[..], b"abcd");
}

#[test]
fn body_frame_ordering_data_then_trailers() {
    let mut trailers = HeaderMap::new();
    trailers.insert("x-end", HeaderValue::from_static("now"));
    let mut body = VecBody::from_chunks([Bytes::from_static(b"abc"), Bytes::from_static(b"de")])
        .with_trailers(trailers);

    let nop = Waker::from(Arc::new({
        struct Nop;
        impl Wake for Nop {
            fn wake(self: Arc<Self>) {}
        }
        Nop
    }));
    let mut cx = Context::from_waker(&nop);

    // First two frames are data, in declared order.
    for expected in [&b"abc"[..], &b"de"[..]] {
        let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut cx) else {
            panic!("expected data frame");
        };
        let buf = frame
            .into_data()
            .unwrap_or_else(|_| panic!("expected data frame"));
        assert_eq!(&buf[..], expected);
    }

    // Then trailers.
    let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut cx) else {
        panic!("expected trailer frame");
    };
    let trailers = frame
        .into_trailers()
        .unwrap_or_else(|_| panic!("expected trailer frame"));
    assert_eq!(
        trailers.get("x-end").map(HeaderValue::as_bytes),
        Some(&b"now"[..])
    );

    // Then end-of-stream.
    assert!(matches!(
        Pin::new(&mut body).poll_frame(&mut cx),
        Poll::Ready(None)
    ));
    assert!(body.is_end_stream());
}

#[test]
fn cancellation_observable_through_clone() {
    let parent = Cancellation::new();
    let child = parent.clone();
    assert!(!parent.is_cancelled());
    assert!(!child.is_cancelled());

    child.cancel();
    assert!(parent.is_cancelled());
    assert_eq!(parent.check(), Err(CancellationError::Cancelled));
}

#[test]
fn cancellation_default_is_uncancelled() {
    let c: Cancellation = Cancellation::default();
    assert!(!c.is_cancelled());
}
