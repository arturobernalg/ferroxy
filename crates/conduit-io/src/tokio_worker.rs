//! Tokio backend — development-only on non-Linux hosts and the
//! comparison runtime under `--features runtime-tokio`.
//!
//! Phase 1 ships a **stub**: the public API is wired so the workspace
//! compiles and the type system is stable, but [`serve`] returns
//! [`crate::BindError::TokioBackendUnimplemented`] at runtime. The full
//! tokio implementation lands alongside the bench harness in Phase 11,
//! where it is needed to drive the nginx/Pingora comparisons.
//!
//! The stub exists so the macOS development build does not bitrot:
//! any change that breaks the cross-platform compile is caught by the
//! macOS CI job rather than discovered weeks later.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpStream;

use crate::{BindError, ServeSpec, ShutdownReport, ShutdownTrigger};

/// Active server (stub variant). Carries no live workers.
#[derive(Debug)]
pub struct Server {
    flag: Arc<AtomicBool>,
    started: Instant,
}

impl Server {
    /// Cheaply cloneable shutdown handle. Wired even though the stub
    /// has nothing to cancel: callers can compile and exercise their
    /// signal-handling code paths on macOS.
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger {
            flag: Arc::clone(&self.flag),
        }
    }

    /// Always empty for the stub; matches the monoio backend's signature.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &[]
    }

    /// Stub `wait`: returns an empty report immediately.
    pub fn wait(self) -> ShutdownReport {
        ShutdownReport {
            elapsed: self.started.elapsed(),
            ..Default::default()
        }
    }
}

/// Stub `serve`. Returns [`BindError::TokioBackendUnimplemented`] for
/// any non-empty `listeners`; an empty `listeners` returns
/// [`BindError::NoListeners`] to keep the validation order consistent
/// with the monoio backend.
//
// The setup closure is accepted but never invoked; this keeps the
// generic bounds identical across backends so callers compile either
// way without `cfg`-conditional handler signatures.
#[allow(clippy::needless_pass_by_value)]
pub fn serve<S, H, Fut>(spec: ServeSpec, _setup: S) -> Result<Server, BindError>
where
    S: Fn() -> H + Send + Sync + 'static,
    H: Fn(TcpStream, SocketAddr) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    if spec.listeners.is_empty() {
        return Err(BindError::NoListeners);
    }
    Err(BindError::TokioBackendUnimplemented)
}
