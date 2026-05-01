# Architecture

This document describes how the **conduit** binary is structured. It is
maintained alongside the code; if a phase ships behaviour that contradicts
this document, fix the document.

## Layer model

The codebase is split across a workspace of layer crates. Dependencies
point **down** the layer model — a higher layer may depend on a lower
one but never the reverse. The direction is enforced in `deny.toml` via
`[[bans.deny]] wrappers = …` allowlists; adding an upward edge fails the
gate.

```
                   ┌──────────────────────────┐
   binary  ───────▶│ conduit (bin)            │
                   └────────────┬─────────────┘
                                │ wires every layer
   layer 6 (control plane)      ▼
                   ┌──────────────────────────┐
                   │ conduit-control          │
                   │ admin / metrics / reload │
                   └────────────┬─────────────┘
                                │
   layer 5 (data plane)         ▼
                   ┌──────────────────────────┐
                   │ conduit-lifecycle        │
                   │ Dispatch / RouteTable    │
                   └────────────┬─────────────┘
                                │
                   ┌──────────────────────────┐
                   │ conduit-upstream         │
                   │ pool / round-robin / retry │
                   └────────────┬─────────────┘
                                │
   layer 3 (protocol)           ▼
                   ┌──────────────────────────┐
                   │ conduit-h1 / h2 / h3     │
                   │ serve_connection per ALPN│
                   └────────────┬─────────────┘
                                │
   layer 2 (transport)          ▼
                   ┌──────────────────────────┐
                   │ conduit-transport        │
                   │ rustls TLS + QUIC server │
                   └────────────┬─────────────┘
                                │
   layer 1 (io)                 ▼
                   ┌──────────────────────────┐
                   │ conduit-io               │
                   │ monoio (Linux) / tokio   │
                   └──────────────────────────┘

   layer 6.5 (config)
                   ┌──────────────────────────┐
                   │ conduit-config           │
                   │ TOML schema + validation │
                   └──────────────────────────┘
```

`conduit-config` is depended on by every layer that needs to read the
TOML document directly (`conduit-control` for orchestration, `conduit-
lifecycle` for `RouteTable::from_config`, `conduit-upstream` for
`RetryPolicy`, `conduit-transport` for `TlsConfig`). The binary loads
the document once and feeds it through.

`conduit-proto` (not pictured for clarity) sits beside the protocol
crates: it owns the request/response stream **type contract** (`Body`,
`BoxBody`, `Response`, etc.) so the protocol crates and `conduit-
lifecycle` agree on shapes without depending on each other.

## Runtime layout

The binary spawns one OS thread per **runtime plane**:

| Thread name        | Runtime               | Drives                           |
|--------------------|-----------------------|----------------------------------|
| `conduit-io-…`     | tokio multi-thread    | plaintext HTTP/1.1 listeners     |
| `conduit-https-rt` | tokio multi-thread    | TLS+ALPN listeners (h1 + h2)     |
| `conduit-h3-rt`    | tokio multi-thread    | QUIC listeners (h3)              |
| `conduit-admin-rt` | tokio current-thread  | admin server (`/health`, `/ready`, `/metrics`) |
| `conduit-signal`   | none (blocking sigwait) | SIGTERM / SIGINT → shutdown    |
| `conduit-reload`   | none (blocking sigwait) | SIGHUP → atomic Dispatch swap  |

Splitting the planes across threads has two benefits:
1. TLS and QUIC handshakes are CPU-heavy; isolating them keeps the
   plaintext data plane's tail latency from getting clobbered by a
   handshake spike.
2. Admin endpoint latency stays decoupled from the data plane under
   load. A liveness probe should never share a runtime with a hot path.

Once the monoio↔tokio bridge ships (P3.x deviation), the plaintext data
plane will run on monoio + io_uring; the HTTPS / H3 / admin planes stay
on tokio because their dependencies (`tokio-rustls`, `quinn`, `hyper`'s
admin server) are tokio-only.

## Hot-path discipline

The charter requires zero allocations and zero `Arc<Mutex<…>>` on the
per-request hot path.

- `Dispatch` is wrapped in `Arc<ArcSwap<Dispatch>>`. Each request loads
  one snapshot `Arc` via `load_full()` and drops it at end-of-request.
  SIGHUP-triggered reloads call `store(new_dispatch)` and never block a
  request.
- The upstream pool (hyper-util's `legacy::Client`) is shared across
  worker threads as a single `Arc`. It internally uses lock-free
  bookkeeping for idle connections.
- Round-robin selection is `Arc<AtomicUsize>::fetch_add(1, Relaxed) %
  len`.
- Bodies traverse the proxy as `http_body::Body<Data = Bytes>` trait
  objects; cloning a `Bytes` is a refcount bump.

The cold paths that *do* allocate (config reload, admin response
construction, error-path 502 builders) are intentionally out of band.

## Layer responsibilities

### `conduit-io` — Layer 1
Bind sockets, accept connections, hand each one to a per-connection
handler. Two backends behind a feature flag: **monoio** (production
Linux, io_uring) and **tokio** (development, comparison). Selected by
the binary at build time via the `runtime-tokio` cargo feature.

### `conduit-transport` — Layer 2
Build a `rustls::ServerConfig` from the `[tls]` block, wrap accepted
TCP streams in `tokio_rustls::TlsAcceptor`, and surface
`accept_tls`. Single cert + ALPN advertising `h2,http/1.1` today;
SNI multi-cert and OCSP stapling are P6.x scope.

### `conduit-h1` / `conduit-h2` / `conduit-h3` — Layer 3
Each crate exposes a `serve_connection(io_or_conn, handler)` function
mirroring the same shape. The binary picks one based on:
- which listener accepted the connection (h1 plaintext, h3 QUIC)
- the negotiated ALPN for TLS connections (h1 fallback / h2)

### `conduit-upstream` — Layer 5 (data plane)
Owns the egress pool. `Upstream::forward` takes a generic-bodied
request, normalises it to HTTP/1.1 (P8 translation rule), buffers the
body for retry replay, picks a backend via round-robin, and replays
through `hyper-util`'s `legacy::Client`.

### `conduit-lifecycle` — Layer 5 (data plane orchestration)
Owns `Dispatch::handle`: takes a request, looks up the route, looks
up the upstream, calls `Upstream::forward`. Routes are matched with a
linear scan today; a prefix trie is P5.x scope.

### `conduit-control` — Layer 6
Admin server with `/health`, `/ready`, `/metrics`. The metrics endpoint
emits Prometheus 0.0.4 text format with `conduit_uptime_seconds` and
`conduit_build_info`. The full per-route counter family arrives once
the data plane has a shared metrics handle (P10.x.x).

### `conduit-config` — schema crate
Parses + validates the TOML document. No I/O, no network. Every
other layer takes already-validated config structs.

## Hot-reload

SIGHUP triggers `install_reload_handler`:

1. Re-read the config file from the path the binary was started with.
2. If the parse + validation succeeds, build a new `Dispatch` and
   atomically `store` it into the shared `ArcSwap`.
3. If it fails, log the error and leave the existing `Dispatch` in
   place — the proxy keeps serving traffic.

In-flight requests that loaded the previous snapshot continue using
their old routes/upstreams. New requests see the new config the
moment `store` completes. There is no drain window; the old snapshot
is dropped when the last request that loaded it finishes.

Listener / TLS / port config changes are **not** hot-reloadable — the
listener thread holds the bound socket. Operationally this is the
nginx model: SIGHUP for content changes, restart for socket changes.
