//! ferroxy-io — Layer 1: listeners, accept loop, worker model, buffer pool.
//!
//! Owns raw byte streams and datagram flows. Knows nothing about HTTP. Hands
//! accepted streams to Layer 2 (`ferroxy-transport`) for ALPN dispatch.
//!
//! Implemented in phase 1 of the build order: monoio runtime,
//! thread-per-core, `SO_REUSEPORT`, `io_uring` registered buffers, CPU
//! pinning. A tokio backend lives behind the `runtime-tokio` cargo
//! feature for portability and comparison benchmarks.
#![deny(missing_docs)]
