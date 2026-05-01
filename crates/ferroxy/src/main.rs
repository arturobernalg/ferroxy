//! ferroxy — reverse proxy binary.
//!
//! Phase 0: this binary loads and validates the configuration, emits a
//! structured log line on success, and exits. The runtime is implemented
//! in phase 1 (`ferroxy-io`, monoio-backed by default).

#![deny(missing_docs)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod log;

// Stable CLI contract: the only positive command in phase 0 is `--check`.
// Running the binary without `--check` prints a structured error and exits
// non-zero, so accidental "default-run" in scripts fails loudly until the
// runtime ships in phase 1.
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
/// - `0` — `--check` succeeded.
/// - `2` — config could not be loaded or failed validation.
/// - `64` — caller asked the binary to *run* but the runtime is not yet
///   implemented in this build (phase 0). Mirrors `EX_USAGE` from
///   sysexits(3) so systemd treats it as a configuration error.
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

    tracing::error!(
        "phase 0 build: runtime is not implemented yet; rerun with --check to validate config",
    );
    ExitCode::from(64)
}

fn print_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("  error: {err}");
    let mut src = err.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}
