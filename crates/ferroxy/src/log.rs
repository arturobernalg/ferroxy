//! Tracing subscriber wiring.
//!
//! Filter follows `RUST_LOG`-style syntax via `FERROXY_LOG`; default is
//! `info`. JSON output is opt-in (CLI flag) so logs stay readable when run
//! interactively but become machine-parseable in production.

use tracing_subscriber::fmt;
use tracing_subscriber::EnvFilter;

pub(crate) fn init(json: bool) {
    let filter = EnvFilter::try_from_env("FERROXY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}
