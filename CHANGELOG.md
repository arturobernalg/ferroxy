# Changelog

Conduit is pre-1.0; this file records what's shipped per phase of the
[engineering charter](./CHARTER.md). Format is loosely
[Keep a Changelog](https://keepachangelog.com/) — an `Unreleased`
section accumulates work that hasn't tagged a release yet.

## Unreleased

### Layer 1 — `conduit-io`
- monoio production backend (Linux, io_uring) and tokio comparison
  backend behind the `runtime-tokio` cargo feature.
- `serve(spec, setup_factory)` accepts a per-listener handler factory
  and runs one worker per core with `SO_REUSEPORT`.

### Layer 2 — `conduit-transport` (TLS)
- TLS termination via `rustls` 0.23 with the `ring` crypto provider.
- SNI multi-cert via `MultiCertResolver`: exact-match HashMap +
  wildcard (`*.foo.com` → one DNS label below) + first-cert fallback
  matching nginx's default-server semantics.
- ALPN advertises `h2,http/1.1` for HTTPS listeners; `h3` for QUIC
  endpoints.
- PEM parsing via `rustls-pki-types::pem::PemObject`
  (replaces the archived `rustls-pemfile` — RUSTSEC-2025-0134).

### Layer 3 — protocol crates
- **`conduit-h1`**: HTTP/1.1 ingress via hyper. `serve_connection`
  takes any tokio-shaped stream + a per-request handler closure.
- **`conduit-h2`**: HTTP/2 ingress via hyper http2; the binary's
  HTTPS listener dispatches by ALPN (`h2` → conduit-h2, anything
  else → conduit-h1).
- **`conduit-h3`**: HTTP/3 ingress via `quinn` 0.11 + `h3` 0.0.8. The
  binary spawns a dedicated tokio runtime + thread for the QUIC
  listener.

### Layer 4 — `conduit-lifecycle` (routing)
- `RouteTable` indexed by host bucket: O(1) host hash, then exact
  path (HashMap) or longest-prefix (length-sorted Vec) or regex
  (declaration-order Vec) or wildcard.
- Path matching: `path_exact`, `path_prefix`, `path_regex`
  (compiled once at config-load).
- Header-based routing: `[[route]] match.headers` predicates;
  routes with failing predicates fall through to less-specific
  candidates at the same path.
- Per-route `timeouts.total` enforced via `tokio::time::timeout`
  (504 to client on expiry).

### Layer 5 — `conduit-upstream`
- HTTP/1.1 client pool via hyper-util's `legacy::Client`,
  upgraded to `hyper-rustls::HttpsConnector` so the same Client
  routes both `http://` and `https://` URIs.
- Round-robin load balancing across an upstream's `addrs`, shared
  across worker threads via an `AtomicUsize` counter.
- Per-addr passive circuit breaker: lock-free `(failures, deadline)`
  state machine; `pick_addr` skips Open backends so a single bad
  one doesn't take down the pool.
- Active health checks: per-upstream prober that GETs the
  configured `health_check.path` on every addr at `interval`,
  feeding outcomes back to the same per-addr breakers. Probes go
  over HTTPS when the upstream is configured for TLS.
- Per-upstream TLS:
  - `tls.ca` — custom CA bundle replaces webpki roots for this
    upstream
  - `tls.client_cert` + `tls.client_key` — mTLS to backend
  - `tls.verify = false` — test-only "trust everything" verifier
    (logs a warning at construction)
- Per-upstream `connect_timeout` wired through hyper-util's
  `HttpConnector::set_connect_timeout`.
- H2 → H1 translation for egress: inbound H2 / H3 requests are
  rewritten to HTTP/1.1 before hitting the pool; pseudo-headers
  are projected onto Request parts upstream.

### Layer 6 — `conduit-control`
- Admin server on its own tokio runtime + thread:
  - `GET /health` — liveness
  - `GET /ready` — readiness
  - `GET /metrics` — Prometheus 0.0.4 text exposition
  - `GET /upstreams` — live pool stats with per-upstream breaker
    state (`closed` / `open` / `half_open`)
- `MetricsHandle` — lock-free `AtomicU64` counters fed by the data
  plane:
  - `conduit_requests_total`
  - `conduit_requests_no_route_total`,
    `conduit_requests_upstream_unknown_total`,
    `conduit_requests_upstream_failed_total`
  - `conduit_responses_total{class="2xx|3xx|4xx|5xx"}`
  - `conduit_request_duration_seconds_*` histogram
    (Prometheus-default buckets, 1ms..10s + +Inf)
  - `conduit_request_duration_seconds_by_class_*` — per-class
    histogram so dashboards can split p99 of successes vs errors
    with one PromQL query
  - `conduit_uptime_seconds`, `conduit_build_info`

### Config validation hardening
- `[upstream.tls]` with `client_cert` XOR `client_key` rejected at
  load (mTLS needs both).
- `[upstream.tls] verify = false` rejected unless
  `CONDUIT_ALLOW_INSECURE_TLS=1` is set in the environment. Forces
  deliberate opt-in instead of letting the "trust everything"
  verifier sneak into a prod config.
- `[upstream.health_check]` `interval = "0s"` / `timeout = "0s"`
  rejected at load (would either spin the prober or fail every
  probe).

### Binary
- `conduit-config` strict TOML schema + validation.
- SIGHUP-driven hot-reload via `arc-swap`: routes / upstreams /
  retry policies swap atomically without a Mutex on the hot path.
  Failed reloads log + leave the previous config running.
- SIGTERM/SIGINT → graceful shutdown across every plane (HTTP,
  HTTPS, H3, admin, health probes, reload thread).
- Structured access logs via `tracing` (text or JSON via
  `--log-json`).

### Hot-path optimisations

A series of measured allocation reductions on the per-request path,
each independently small but stacking up. See
[`bench/micro/README.md`](./bench/micro/README.md) for the
`route_lookup` baseline that gates regressions.

- **`Dispatch::handle`**: dropped 4 allocations per request (host
  String, path String, HeaderMap clone) by keeping the request
  borrows alive only as long as the route lookup needs them.
- **`RouteTable::find`**: `lookup_bucket` skips
  `to_ascii_lowercase` allocation when the host header is already
  lowercase. Bench: 40 → 25 ns first-prefix, 52 → 44 ns last-prefix,
  33 → 25 ns wildcard fallback.
- **`Upstream::forward`**: retry loop no longer clones `parts` on
  the no-retry path (the default policy). Single-attempt forwards
  are a clean move into `http::Request::from_parts`.
- **`Upstream::forward`**: URI rewrite uses `Uri::from_parts` instead
  of `format!() + .parse()`. One allocation instead of three, no
  reparse.
- **`UpstreamMap`**: entries stored as `Arc<Upstream>` so the
  hot-path clone in `Dispatch::handle` is a refcount bump rather
  than a deep copy of `RetryPolicy.on_status` + 4 inner Arcs.
- **`MetricsHandle`**: histogram storage switched from cumulative
  to non-cumulative — each observation bumps exactly one bucket.
  Cumulative sums computed at scrape time. Per-request atomic ops
  on the histogram path: ~24 → 2.

### Build / quality / docs
- Workspace-wide quality gate: `cargo build --workspace --all-targets
  --release`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check`, `cargo deny check`.
- Layer-direction enforcement via `[[bans.deny]] wrappers = …`
  in `deny.toml`. Wrapper lists trimmed to actually-used dependents
  to keep `cargo deny check` warning-free.
- Microbench: `route_lookup` (37 ns / 50 ns / 32 ns at 100 hosts ×
  10 prefixes — see [`bench/micro/README.md`](./bench/micro/README.md)).
- cargo-fuzz target `h1_parser` with 12 hand-curated seeds in
  `fuzz/seeds/h1_parser/` (CRLFs preserved via `.gitattributes`).
- Comparison harness scaffolded at `bench/compare/` with a
  methodology gate that refuses to run on loopback / kernel < 6.6 /
  unset NIC. Single-machine `bench/run.sh` is regression-only.
- Docs: [`ARCHITECTURE.md`](./ARCHITECTURE.md),
  [`CONFIG.md`](./CONFIG.md), [`BENCHMARKS.md`](./BENCHMARKS.md),
  refreshed `examples/conduit.toml` covering every shipped knob.
- Multi-arch CI (Linux + macOS dev path); `linux/amd64` and
  `linux/arm64` Docker image targets. `aarch64-unknown-linux-musl`
  temporarily disabled until monoio/libc upstream the `libc::statx`
  fix.

### Deferred (charter-tracked)

These items are charter scope but not yet shipped. Each is tracked
in the project's `phaseN_deviations` notes:

- **Streaming bodies** for the upstream client and H3 ingress (P8.x / P9.x).
- **Per-leg read/write timeouts** beyond the connect timeout (P5.x.x).
- **OCSP stapling**, session-ID cache, certificate hot-reload (P6.x.x).
- **0-RTT for QUIC** (P9.x).
- **`h2spec` / `qlog` CI gates** (P7.x / P9.x).
- **Upstream H2 / H3 client** — backends still get HTTP/1.1 (P7.x / P9.x).
- **monoio↔tokio bridge** so the binary can drive monoio's io_uring
  backend through hyper (P3.x).
- **bumpalo per-request arena** for header storage (P3.x).
- **Per-worker connection-pool sharding** to replace hyper-util's
  shared `legacy::Client` (P4.x; gated on a P11.5 profile).
- **OpenTelemetry tracing** with W3C `traceparent` propagation
  (P10.x.x).
- **Real benchmark numbers vs nginx / Pingora** — gated on a real
  bench box per [`BENCHMARKS.md`](./BENCHMARKS.md). Honest current
  status: architecturally plausible, empirically unproven.
