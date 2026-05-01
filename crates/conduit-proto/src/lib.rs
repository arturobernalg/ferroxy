//! conduit-proto — the request/response stream contract.
//!
//! Per the engineering charter this is the **one** real abstraction in
//! the system: every protocol crate (`conduit-h1`, `conduit-h2`,
//! `conduit-h3`) maps its wire format onto these types, and
//! `conduit-lifecycle` consumes them. The contract models exactly:
//!
//! - the **head** ([`Request<B>`] / [`Response<B>`] from the `http` crate),
//! - the **streaming body** ([`http_body::Body`] yielding `Bytes` chunks),
//! - the **trailers** (a final [`http_body::Frame::trailers`]),
//! - the **cancellation token** ([`Cancellation`]) — a runtime-agnostic
//!   `Arc<AtomicBool>` that any side can flip to signal "stop",
//! - the **half-close** semantic, which the `Body` trait already
//!   expresses via `is_end_stream` + the natural ordering of frames.
//!
//! Nothing more. There is no built-in routing concept here, no
//! response-builder DSL, no protocol-specific type. Those belong to
//! the layers that own them.
//!
//! # Runtime independence
//!
//! This crate is intentionally free of `tokio` and `monoio` references.
//! It compiles unchanged on every supported target. Async-runtime
//! integration (poll-driving, task spawning) is the consumer's job.
//!
//! # Hot-path discipline
//!
//! `Request<B>` / `Response<B>` are generic over `B`; consumers (the
//! lifecycle layer, the protocol crates) monomorphise per call site,
//! avoiding the `Box<dyn Body>` allocation per request that the charter
//! forbids on the hot path. Dispatch through erased `BoxBody` is
//! permitted on cold paths only (admin endpoints, error responses).

#![deny(missing_docs)]

mod body;
mod cancel;

use bytes::Bytes;

pub use body::{boxed, BoxBody, VecBody};
pub use cancel::{Cancellation, CancellationError};

// Re-export the http types we standardise on so callers do not pin
// their own version of `http` and risk a major-version mismatch.
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version};
pub use http_body::{Body, Frame, SizeHint};

/// A request flowing through the proxy. `B` is the request body type;
/// each protocol crate provides its own concrete `B` that implements
/// [`Body<Data = Bytes>`].
pub type Request<B> = http::Request<B>;

/// A response flowing through the proxy. Same generic shape as
/// [`Request`].
pub type Response<B> = http::Response<B>;

/// Convenience alias used by the lifecycle layer when it needs a
/// type-erased body (e.g. when forwarding a request whose body type
/// differs from the response body type). Boxing happens once per
/// request in this case, on a cold path.
pub type ErasedRequest = Request<BoxBody>;

/// Counterpart to [`ErasedRequest`].
pub type ErasedResponse = Response<BoxBody>;

/// Required body bound: yields `Bytes` chunks. This bound exists so
/// call sites can write `B: BodyBytes` instead of `B: Body<Data = Bytes>`.
pub trait BodyBytes: Body<Data = Bytes> {}

impl<B> BodyBytes for B where B: Body<Data = Bytes> {}
