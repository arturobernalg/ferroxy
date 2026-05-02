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
- **`ProxyBody` enum** replaces the per-request `BoxBody`
  allocation in the binary. Two variants (`Upstream(Incoming)`,
  `Synthetic(VecBody)`) cover every body the proxy produces; the
  compiler can inline through the enum dispatch and branch
  prediction sees one variant per response so the cost is ~free.
  One `Box<dyn Body>` heap allocation eliminated per request.
- **`Upstream::forward`**: skip `body.collect()` when the request
  body is known empty (`size_hint().exact() == Some(0)`). Most
  GET / HEAD traffic skips the buffer-then-replay step entirely.
- **Pre-built `Authority` per addr**: `Upstream` now caches an
  `Arc<[http::uri::Authority]>` parallel to `addrs`, so per-request
  URI rewrites just clone (refcount bump on the inner `Bytes`)
  instead of running `addr.to_string()` + parse on every call.

### QUIC 0-RTT (opt-in)

`[tls] enable_0rtt = true` sets rustls' `max_early_data_size` to
16 KiB on the server config. Quinn's
`QuicServerConfig::try_from(rustls_cfg)` picks it up; the H3
listener accepts TLS 1.3 early data on resumed sessions.

Charter rule: 0-RTT is **off by default** because early data is
replayable by an on-path attacker. Enabling it surfaces a startup
warning so operators have to acknowledge that handlers must be
idempotent.

### TLS extras: session tickets + OCSP stapling

- **Session ticket resumption (TLS 1.2)**: rustls' default ticketer
  is `NeverProducesTickets`, so without a config every TLS 1.2
  client paid the full handshake cost on every connection.
  `load_server_config` now installs `rustls::crypto::ring::Ticketer::new()`
  — a 12-hour rotating ChaCha20Poly1305 ticketer (rustls' recommended
  configuration). TLS 1.3 has its own resumption (PSK) and
  needs no opt-in here.
- **OCSP stapling**: `[[tls.certs]] ocsp_response` accepts an
  optional path to a pre-fetched DER OCSP response. Loaded into
  the `CertifiedKey` and stapled into handshakes by rustls.
  Operators refresh the file out-of-band (e.g. certbot's
  `--deploy-hook`) and SIGHUP picks it up.

### TLS cert hot-reload + upstream HTTP/2 ALPN

`MultiCertResolver` now wraps its `(exact, wildcard, fallback)`
tables in `arc_swap::ArcSwap`. Calling `reload(specs)` re-reads
every PEM and atomically swaps the table; the next handshake's
`ResolvesServerCert::resolve` picks the new entry. Failed reload
(missing/bad PEM) leaves the previous table active and just logs.

The binary's SIGHUP handler now reloads in this order:
1. Re-read config from disk.
2. Build N+1 fresh `Dispatch`es (1 primary + N HTTP-worker), atomic
   `store` into each swappable.
3. If HTTPS is configured: `cert_resolver.reload(&cfg.tls.certs)`
   to swap the cert table.

`load_server_config_with_resolver` is the new entry point for
callers that want the resolver handle. The convenience
`load_server_config` is the same minus the handle (back-compat).

Upstream egress connector now advertises `h2,http/1.1` in TLS
ALPN. h2-capable backends opportunistically negotiate H/2; plain
and h1-only TLS backends keep getting h1. Wiring h2 actually used
on the wire (rather than just advertised) is the deferred follow-up.

### Per-worker `Dispatch` — pool sharding done

Each HTTP plain plane worker now holds its own `Dispatch`,
`UpstreamMap`, and `Upstream::client`. The hyper-util
`Mutex<Pool>` is no longer shared across worker threads; cross-
worker contention on connection acquisition is gone for the
highest-RPS path.

Implementation:
- `DispatchBundle` holds one `primary` swappable + N
  `http_workers` swappables.
- HTTPS / H3 / admin / health / reload threads use the primary
  (each is on its own dedicated thread anyway).
- `start_http_server`'s `setup` closure pulls the next
  `http_worker` via a `fetch_add` counter, so each worker thread
  gets one slot.
- SIGHUP reload updates **every** swappable (1 + N) atomically —
  both the shared dispatch and the per-worker dispatches see
  the new config. Failed reload leaves them all untouched.

Smoke-tested with workers=4 × 30 concurrent requests; all served,
metrics agree, `/upstreams` renders the live state.

### Per-worker tokio backend

`conduit-io::tokio_worker` was a single multi-thread runtime
sharing one accept loop per listener and one
`hyper-util::Client` across every worker. Replaced with N OS
threads, one per worker, each running its own current-thread
tokio runtime. Listeners are bound N times per address with
`SO_REUSEPORT` so the kernel distributes connections across
workers; `setup()` is called once per worker per listener so
handlers can capture per-worker state.

The two backends (tokio + monoio) now differ only in runtime
builder, listener type, and counter type — same per-worker shape
otherwise. This unblocks per-worker Client / per-worker pool
sharding (next step in the deferred section).

Smoke-tested with workers=4 and 20 concurrent requests: all 20
served, metrics agree.

### Per-leg read timeout (`TimedBody`)

`Upstream::forward` now wraps the upstream's response body with
`TimedBody` — a per-frame read deadline derived from
`[[route]] timeouts.read`. A slow / silent backend can no longer
tie up a conduit connection past the configured deadline regardless
of the total route budget.

Plumbed end-to-end:
- `Route` carries `read_timeout: Duration` from
  `cfg.timeouts.read.into()`.
- `Dispatch::handle` returns `Response<TimedBody<Incoming>>`.
- `ProxyBody::Upstream` now stores `TimedBody<Incoming>` and uses
  `pin_project_lite` to forward pin access (TimedBody's inner
  `tokio::time::Sleep` is `!Unpin`).
- `conduit_h3::serve_connection` dropped its `B: Unpin` bound and
  stack-pins the response body so non-`Unpin` body types work
  without an extra `Box::pin` allocation.

Tests: `timer_fires_when_inner_stalls` (50ms timeout against a
forever-pending body returns `ReadTimeout`) and `ends_cleanly_on_eos`
(a Full body completes without firing the timer).

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

These items are charter scope but not yet shipped. Each is
either substantial multi-day work or genuinely
infrastructure-gated. Tracked in the per-phase deviation notes.

**Substantial code work:**

- **Streaming request and response bodies** — today the upstream
  request body is buffered into `Bytes` so retry replay works,
  and the H3 ingress drains the request stream before invoking
  the handler. A streaming path requires a different retry model
  (replay buffer with a size cap, or no-replay-after-bytes-sent)
  and a streaming Body impl over h3's `recv_data` (P8.x / P9.x).
- **Upstream H2 / H3 actually on the wire** — the egress
  connector advertises `h2,http/1.1` in TLS ALPN, and the schema's
  `protocol = "h2"` is parsed; the version-forcing in
  `Upstream::forward` still pins requests to HTTP/1.1. Wiring real
  H2 egress needs a per-upstream connector + version split (P7.x).
- **Per-leg write timeout** — the wire-write happens inside
  hyper-util's connector, with no clean external wrap point. The
  per-route `total` timeout already bounds a slow upstream send
  in practice; a true per-leg write timeout would require forking
  the connector or contributing it upstream (P5.x.x.x).
- **monoio↔tokio bridge** — the binary needs `monoio_compat` to
  expose `monoio::net::TcpStream` as a tokio `AsyncRead +
  AsyncWrite` so hyper can drive it. The dep is in the workspace;
  the bridge wiring is the missing piece. Real win on Linux
  io_uring under load (P3.x).
- **bumpalo per-request arena** for header storage — needs hyper
  internals access; a shippable path runs alongside the P11.6
  hyper-replacement gate, both conditional on a real profile (P3.x).

**Infrastructure-gated:**

- **`h2spec` / `qlog` CI gates** (P7.x / P9.x). Need the binary
  running in CI with a known good baseline; tracked alongside the
  bench-box plan.
- **OpenTelemetry tracing** with W3C `traceparent` propagation
  (P10.x.x). The `traceparent` and `tracestate` headers already
  pass through unchanged; emitting OTLP spans linked to the
  inbound trace needs an `opentelemetry` exporter dep tree and a
  collector deployment to be useful.
- **Real benchmark numbers vs nginx / Pingora** — gated on a real
  bench box per [`BENCHMARKS.md`](./BENCHMARKS.md). Honest current
  status: architecturally plausible, empirically unproven.
