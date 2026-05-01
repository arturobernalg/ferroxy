# Conduit

A correctness-first HTTP reverse proxy for Linux, written in Rust.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust: 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
<!-- Add once a release is published:
[![Crates.io](https://img.shields.io/crates/v/conduit.svg)](https://crates.io/crates/conduit)
-->
<!-- Add once .github/workflows/ci.yml lands (Phase 12):
[![CI](https://github.com/arturobernalg/conduit/actions/workflows/ci.yml/badge.svg)](https://github.com/arturobernalg/conduit/actions/workflows/ci.yml)
-->

## Overview

Conduit is an HTTP reverse proxy designed for the modern Linux server: thread-per-core,
share-nothing, `io_uring`-based, with a strict layered architecture in which each crate owns
exactly one concern. The code base is small on purpose — abstractions arrive only when they
remove demonstrable complexity.

The design thesis is that a proxy built from the kernel up against a fixed target workload, with
hot-path discipline enforced at review time and dependency direction enforced at build time, can
match the throughput and tail-latency floor of the established proxies on that workload while
remaining auditable end-to-end. Conduit is the execution of that thesis.

What Conduit is **not**:

- **Not a Web server.** It does not serve files, render templates, or run scripts.
- **Not a cache.** Response caching is out of scope for v1.
- **Not a module platform.** There is no plug-in API; behaviour is configured, not loaded.
- **Not portable.** Production targets are Linux 6.6+ on x86_64 and aarch64. Other platforms
  may build but are unsupported.

## Status

Pre-1.0. Active development. Each phase of the build plan ships only after the full quality gate
is green; the [Roadmap](#roadmap) below reflects actual state, not aspiration. The binary
currently loads and validates a configuration and accepts plain TCP connections via the io
layer; it cannot yet serve HTTP traffic.

## Supported Platforms

Conduit is a Linux-first project. Production deployments are supported on Linux only. Other
operating systems are supported for development.

### Production

Release binaries are published for:

| Target                      | Notes                              |
|-----------------------------|------------------------------------|
| `x86_64-unknown-linux-gnu`  | glibc, most distros                |
| `x86_64-unknown-linux-musl` | static, Alpine                     |
| `aarch64-unknown-linux-gnu` | glibc, ARM64                       |
| `aarch64-unknown-linux-musl`| static, ARM64 (planned, see below) |

> `aarch64-unknown-linux-musl` is currently disabled in CI: a `libc::statx`
> symbol that monoio 0.2.4 references is not exposed by libc 0.2.186 on
> that target. The target will return once monoio or libc upstream the
> fix.

A multi-arch Docker image (`linux/amd64`, `linux/arm64`) is also published.

### Development

The `runtime-tokio` cargo feature provides a portable backend that builds and runs on macOS for
local development. Tests pass in CI on macOS. Performance characteristics on macOS differ from
Linux production; benchmark results in [`BENCHMARKS.md`](./BENCHMARKS.md) apply to Linux only.

```bash
# On macOS, the tokio backend is auto-selected:
cargo build
# To force the tokio backend on Linux (e.g. for comparison benchmarks):
cargo build --features runtime-tokio
```

### Not Supported

Windows is not a supported target. Conduit may or may not build on Windows; if it does, it is
not tested and not recommended for any use.

## Features

Items marked **(planned)** are part of the v1 plan but not yet implemented.

**Protocols**
- HTTP/1.1 (RFC 9110, RFC 9112) with chunked transfer encoding, trailers, and pipelining on
  ingress  **(planned)**
- HTTP/2 (RFC 9113) with HPACK, flow control, and `h2spec` 100% server-side  **(planned)**
- HTTP/3 (RFC 9114, QPACK RFC 9204) over QUIC, with connection migration and `Alt-Svc`
  advertisement  **(planned)**
- TLS 1.2 / 1.3 termination via `rustls` with `aws-lc-rs` as the crypto provider  **(planned)**
- Mutual TLS to upstream  **(planned)**

**Routing**
- Host-based virtual hosts  **(planned)**
- Path matching: prefix, exact, regex  **(planned)**
- Header-based routing  **(planned)**

**Load balancing**
- Round-robin, least-connections, power-of-two-choices, consistent hash  **(planned)**

**Resilience**
- Active health checks (HTTP probes) and passive ejection on consecutive failures  **(planned)**
- Per-upstream circuit breaker  **(planned)**
- Configurable retry policy (status codes, connect errors, attempt count)  **(planned)**
- Separate timeouts: connect, read, write, total  **(planned)**

**Observability**
- Prometheus metrics endpoint  **(planned)**
- OpenTelemetry tracing with W3C `traceparent` propagation  **(planned)**
- Structured access logs (text or JSON)  **(planned)**

**Operations**
- TOML configuration with strict validation
- Hot config reload via `SIGHUP`  **(planned)**
- Graceful shutdown via `SIGTERM` / `SIGINT`
- Admin endpoints (`/health`, `/ready`, `/config`, `/pools`, `/reload`)  **(planned)**
- `systemd`-ready service unit  **(planned)**

## Architecture

Conduit is split into single-concern crates organised as a strict downward stack. Each crate may
depend only on the layers below it; the rule is enforced by `cargo deny check` via
`[[bans.deny]]` entries in [`deny.toml`](./deny.toml). Cross-cutting concerns (tracing) are
threaded through layers via explicit context, never thread-locals.

```
+----------------------------------+
|  control   (config, admin, obs)  |
+----------------------------------+
|  lifecycle (route, filter, lb)   |
+----------------------------------+
|  upstream  (pool, health, cb)    |
+----------------------------------+
|  protocol  (h1, h2, h3)          |
+----------------------------------+
|  transport (tls, quic)           |
+----------------------------------+
|  io        (listen, accept)      |
+----------------------------------+
```

The `conduit-proto` crate sits beside the protocol layer and exposes the single shared
request/response stream type that every protocol implementation maps to and that the lifecycle
layer consumes; it is the one explicit abstraction that the project pays for. See
[`ARCHITECTURE.md`](./ARCHITECTURE.md) for the full design treatment (forthcoming).

## Quick Start

### Build from source

Conduit targets Rust stable. The minimum supported Rust version is **1.75**.

```bash
git clone https://github.com/arturobernalg/conduit.git
cd conduit
cargo build --release
```

The binary is produced at `target/release/conduit`.

### Run with a minimal configuration

Save the following to `conduit.toml`:

```toml
[server]
runtime         = "monoio"
listen_http     = ["0.0.0.0:8080"]
admin_listen    = "127.0.0.1:9090"
metrics_listen  = "127.0.0.1:9091"

[[upstream]]
name      = "origin"
addrs     = ["127.0.0.1:9000"]
protocol  = "http1"

[[route]]
match    = { host = "localhost", path_prefix = "/" }
upstream = "origin"
```

Validate it without starting the runtime:

```bash
conduit --config conduit.toml --check
```

Start it:

```bash
conduit --config conduit.toml
```

### Verify it works

With a backend listening on `127.0.0.1:9000` (e.g. `python3 -m http.server 9000`):

```bash
curl -i http://127.0.0.1:8080/
```

> The HTTP forwarding path is implemented in Phase 3 (`conduit-h1`). Until that phase lands,
> the binary accepts the TCP connection and closes it without speaking HTTP.

## Configuration

Conduit is configured via a single TOML file. The schema is strictly validated at load: unknown
keys are rejected, semantic constraints (route targets exist, certificate files exist, etc.) are
checked before the runtime starts. A complete example with TLS, multiple routes, and health
checks:

```toml
[server]
runtime         = "monoio"
workers         = "auto"
cpu_affinity    = true
listen_http     = ["0.0.0.0:80"]
listen_https    = ["0.0.0.0:443"]
admin_listen    = "127.0.0.1:9090"
metrics_listen  = "127.0.0.1:9091"

[tls]
min_version = "1.3"
alpn        = ["h2", "http/1.1"]
certs = [
    { sni = "api.example.com",   cert = "/etc/conduit/api.crt",      key = "/etc/conduit/api.key" },
    { sni = "*.static.example.com", cert = "/etc/conduit/static.crt", key = "/etc/conduit/static.key" },
]

[[upstream]]
name             = "api"
addrs            = ["10.0.0.10:8080", "10.0.0.11:8080", "10.0.0.12:8080"]
protocol         = "h2"
lb               = "p2c"
connect_timeout  = "2s"
pool             = { max_idle_per_host = 64, idle_timeout = "60s", max_lifetime = "10m" }
health_check     = { path = "/healthz", interval = "5s", unhealthy_threshold = 3 }

[[upstream]]
name      = "static"
addrs     = ["10.0.0.20:8080"]
protocol  = "http1"
lb        = "round_robin"

[[route]]
match    = { host = "api.example.com", path_prefix = "/" }
upstream = "api"
timeouts = { connect = "2s", read = "30s", write = "30s", total = "60s" }
retry    = { attempts = 2, on_status = [502, 503, 504], on_connect_error = true }

[[route]]
match    = { host = "static.example.com", path_prefix = "/" }
upstream = "static"
```

The annotated reference example lives at [`examples/conduit.toml`](./examples/conduit.toml). The
full key-by-key reference will be published as [`CONFIG.md`](./CONFIG.md) (forthcoming).

## Performance

Conduit is engineered against a fixed target workload, defined in the project charter and
reproducible from the benchmark harness:

- Linux 6.6+, x86_64, 16 cores, 32 GB RAM, 25 Gbps NIC
- HTTP/2 ingress, HTTP/1.1 to upstream, 90/10 mix
- Request bodies <1 KB p95; response bodies 1–50 KB p95
- 10 000 concurrent client connections, 500 000 requests/sec sustained
- Proxy-overhead latency targets: p50 <1 ms, p99 <5 ms, p999 <20 ms

The win condition is to **match or beat nginx (latest stable) and Pingora (current main, default
config) on RPS, p99 latency, and RPS-per-core on this exact profile, on the same hardware,
kernel sysctls, and day**, with all three proxies driven by `bench/run.sh`.

Benchmark results against nginx and Pingora on the target workload profile are tracked in
[`BENCHMARKS.md`](./BENCHMARKS.md) and updated on each release. No numbers are published before
Phase 11 of the build plan completes.

## Building and Testing

### Toolchain

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
```

### Required tools

```bash
cargo install --locked cargo-deny cargo-fuzz cargo-criterion cargo-show-asm
```

(The `cargo-asm` subcommand is provided by `cargo-show-asm`; the original `cargo-asm` crate is
unmaintained.)

### Local quality gate

The same five steps run in CI; every contribution must pass them locally first.

```bash
cargo build   --workspace --all-targets --release
cargo test    --workspace
cargo clippy  --workspace --all-targets -- -D warnings
cargo fmt     --all -- --check
cargo deny    check
```

### Integration tests, fuzzing, benchmarks

```bash
# Integration tests for a single crate
cargo test -p conduit-io --test echo

# Fuzz target (lands in Phase 3 with the H1 parser)
cargo fuzz run h1_parser -- -max_total_time=60

# Microbenchmarks (lands in Phase 11)
cargo criterion --workspace
```

## Roadmap

<a id="roadmap"></a>

### Done

- **Phase 0** — Workspace skeleton, `conduit-config` (parse + validate), CLI, structured
  logging, dependency-direction lints in `deny.toml`.

### In progress

- **Phase 1** — `conduit-io`: monoio listener, thread-per-core worker model with
  `SO_REUSEPORT`, CPU pinning, `io_uring` (registered buffers deferred to a P1.x cleanup pass),
  graceful shutdown on `SIGTERM` / `SIGINT`.

### Planned (v1)

- **Phase 2** — `conduit-proto`: shared request/response stream contract with property tests.
- **Phase 3** — `conduit-h1`: parse, serialize, end-to-end forward to a mock upstream;
  per-request `bumpalo` arena.
- **Phase 4** — `conduit-upstream` (h1): sharded per-worker pool, health checks, passive
  ejection, circuit breaker, retry.
- **Phase 5** — `conduit-lifecycle`: prefix-trie route table built at config load, filter
  chain, timeout enforcement.
- **Phase 6** — `conduit-transport`: TLS via `rustls` + `aws-lc-rs`, SNI, certificate
  hot-reload via `arc-swap`.
- **Phase 7** — `conduit-h2`: server then client; `h2spec` 100% server-side; HPACK fuzz target.
- **Phase 8** — Protocol translation (H2 ingress → H1 egress) preserving trailers,
  cancellation, and back-pressure.
- **Phase 9** — `conduit-h3`: QUIC + H3 with interop against `quiche` and `ngtcp2`; 0-RTT
  off by default.
- **Phase 10** — `conduit-control`: `SIGHUP` hot-reload, admin endpoints, full Prometheus
  metrics, OpenTelemetry tracing wiring.
- **Phase 11** — Benchmark harness in `bench/`: `docker-compose` driving Conduit, nginx, and
  Pingora against four upstream servers under identical sysctls, worker counts, and cipher
  suites.
- **Phase 11.5** — Profile against the target workload; identify the top five hot functions;
  publish findings to `PROFILING.md`.
- **Phase 11.6** *(conditional)* — Replace `hyper` on the hot path with direct H1/H2 state
  machines on monoio I/O traits if Phase 11.5 numbers justify the rewrite.
- **Phase 12** — Production polish: Dockerfile (multi-stage, distroless), `systemd` unit, full
  CI workflow (`.github/workflows/ci.yml`), `ARCHITECTURE.md`, `CONFIG.md`, `BENCHMARKS.md`,
  `CONTRIBUTING.md`, `LICENSE-APACHE`, `LICENSE-MIT`.

### Future (post-v1)

- Response caching with revalidation primitives.
- Rate-limiting and connection throttling at the lifecycle layer.
- Plug-in story for filters (only after the v1 filter API has stabilised under real load).
- Additional crypto providers behind a cargo feature.
- Non-Linux portability builds via the `runtime-tokio` cargo feature.

## Contributing

Contributions are welcome. Conduit is a serious project with a written engineering charter that
contributors are signing up for: one trait per real concept, no speculative generality, hot-path
discipline (no per-request allocation, no `Arc<Mutex<…>>`, no async-fn boxing), and one concern
per commit.

Pull requests must pass the full local quality gate before review:

```bash
cargo build   --workspace --all-targets --release
cargo test    --workspace
cargo clippy  --workspace --all-targets -- -D warnings
cargo fmt     --all -- --check
cargo deny    check
```

Hot-path changes additionally require a benchmark in `bench/micro/` showing no regression
against the previous milestone (or a commit message explaining the win). See
[`CONTRIBUTING.md`](./CONTRIBUTING.md) for the full process (forthcoming with Phase 12).

## Security

Please **do not file public GitHub issues for security vulnerabilities**.

Report them via [GitHub's private vulnerability reporting](https://github.com/arturobernalg/conduit/security/advisories/new)
or by email to the project maintainer (address listed in the GitHub profile). A coordinated
disclosure window will be agreed before any public discussion of the issue.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
