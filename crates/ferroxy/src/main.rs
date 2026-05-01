//! ferroxy — reverse proxy binary.
//!
//! Phase 1: loads + validates configuration, builds a per-worker monoio
//! `io_uring` runtime via `ferroxy-io`, and accepts plain-HTTP TCP
//! connections with a placeholder no-op handler. SIGTERM/SIGINT trigger
//! graceful shutdown. HTTP parsing arrives in phase 3, TLS in phase 6,
//! HTTP/3 in phase 9.

#![deny(missing_docs)]

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use ferroxy_io::{serve, ServeSpec, ShutdownReport};

mod log;

// Stable CLI contract. Doc here is internal; user-facing description lives
// in clap's `about` attribute below.
#[derive(Debug, Parser)]
#[command(name = "ferroxy", version, about = "Reverse proxy")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short = 'c', long, value_name = "PATH")]
    config: PathBuf,

    /// Validate the configuration and exit. Does not start the runtime.
    #[arg(long)]
    check: bool,

    /// Emit logs as JSON instead of the default text format.
    #[arg(long)]
    log_json: bool,
}

/// Exit codes are stable for use by init systems and CI:
///
/// - `0` — clean shutdown after a SIGTERM/SIGINT, or `--check` succeeded.
/// - `1` — runtime build failure or unexpected serve error.
/// - `2` — config could not be loaded or failed validation.
/// - `78` — config is structurally valid but has nothing this build can
///   serve (only HTTPS / H3 listeners while we are still in phase 1).
fn main() -> ExitCode {
    let cli = Cli::parse();
    log::init(cli.log_json);

    let cfg = match ferroxy_config::load(&cli.config) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ferroxy: invalid config at {}", cli.config.display());
            print_chain(&err);
            return ExitCode::from(2);
        }
    };

    tracing::info!(
        upstreams = cfg.upstreams.len(),
        routes = cfg.routes.len(),
        listen_http = cfg.server.listen_http.len(),
        listen_https = cfg.server.listen_https.len(),
        listen_h3 = cfg.server.listen_h3.len(),
        "config valid",
    );

    if cli.check {
        return ExitCode::SUCCESS;
    }

    if !cfg.server.listen_https.is_empty() {
        tracing::warn!(
            count = cfg.server.listen_https.len(),
            "HTTPS listeners ignored: TLS termination ships in phase 6",
        );
    }
    if !cfg.server.listen_h3.is_empty() {
        tracing::warn!(
            count = cfg.server.listen_h3.len(),
            "H3 listeners ignored: HTTP/3 ships in phase 9",
        );
    }
    if cfg.server.listen_http.is_empty() {
        tracing::error!(
            "no plain-HTTP listeners; phase 1 has nothing to serve. \
             Add at least one entry to server.listen_http or wait for phase 6 (HTTPS).",
        );
        return ExitCode::from(78);
    }

    if matches!(cfg.server.runtime, ferroxy_config::RuntimeMode::Tokio) {
        tracing::error!(
            "runtime = \"tokio\" requires building ferroxy with --features runtime-tokio; \
             that backend ships alongside the monoio default in phase 1.x. \
             For now, set runtime = \"monoio\".",
        );
        return ExitCode::from(78);
    }

    let workers = resolve_workers(cfg.server.workers);
    let mut spec = ServeSpec::new(cfg.server.listen_http.clone(), workers);
    spec.cpu_affinity = cfg.server.cpu_affinity;

    let server = match serve(spec, || {
        // Phase-1 placeholder handler factory. Each worker calls it once
        // to get its connection handler. The handler reads nothing,
        // writes nothing, just drops the stream — there is no protocol
        // yet. The H1 parser in phase 3 replaces this.
        |_stream: ferroxy_io::MonoioTcpStream, _peer: std::net::SocketAddr| async move {
            // Intentionally empty: accept-and-close.
        }
    }) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "bind failed");
            print_chain(&e);
            return ExitCode::from(1);
        }
    };
    tracing::info!(addrs = ?server.local_addrs(), "listening");

    let trigger = server.shutdown_trigger();

    // Signal handler thread: first SIGTERM/SIGINT triggers graceful
    // shutdown. Subsequent signals fall through to the OS default
    // disposition (process termination), which is the right escalation
    // for an unresponsive shutdown.
    let signal_received = Arc::new(AtomicBool::new(false));
    let signal_received_for_thread = Arc::clone(&signal_received);
    let signal_thread = std::thread::Builder::new()
        .name("ferroxy-signal".into())
        .spawn(move || install_signal_handler(&trigger, &signal_received_for_thread))
        .expect("spawn signal handler thread");

    let report: ShutdownReport = server.wait();

    // The signal-handler thread loops on the same flag; once shutdown
    // started, it has already exited. Joining is best-effort cleanup.
    signal_received.store(true, Ordering::Release);
    let _ = signal_thread.join();

    tracing::info!(
        accepted = report.accepted,
        completed = report.completed,
        elapsed_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        "shutdown complete",
    );
    if report.accepted == report.completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn resolve_workers(w: ferroxy_config::Workers) -> NonZeroUsize {
    match w {
        ferroxy_config::Workers::Auto => std::thread::available_parallelism()
            .unwrap_or_else(|_| NonZeroUsize::new(1).expect("1 is non-zero")),
        ferroxy_config::Workers::Count(n) => n,
    }
}

/// Wait for SIGTERM or SIGINT (cooperative cross-thread via signal-hook),
/// then trigger graceful shutdown. Returns when the first signal arrives
/// or when the parent flags shutdown via `parent_done`.
fn install_signal_handler(trigger: &ferroxy_io::ShutdownTrigger, parent_done: &AtomicBool) {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGTERM, SIGINT]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM/SIGINT handlers");
            return;
        }
    };
    // Iterator blocks until a signal arrives. We only care about the
    // first one; subsequent signals get the kernel's default
    // disposition once we exit this thread.
    if let Some(signal) = signals.forever().next() {
        if parent_done.load(Ordering::Acquire) {
            return;
        }
        match signal {
            SIGTERM => tracing::info!("received SIGTERM; initiating graceful shutdown"),
            SIGINT => tracing::info!("received SIGINT; initiating graceful shutdown"),
            other => {
                tracing::info!(
                    signal = other,
                    "received signal; initiating graceful shutdown"
                );
            }
        }
        trigger.cancel();
    }
}

fn print_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("  error: {err}");
    let mut src = err.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}
