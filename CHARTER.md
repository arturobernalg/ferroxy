# Conduit Engineering Charter

This document is the contract every contribution is judged against. It is deliberately short.
The full per-phase plan lives in this file's history and in the project memory; what follows is
the load-bearing surface that does not change between phases.

## Principles

1. Solve the real problem; state the invariant before writing the code.
2. Logic lives in its owning layer. A fix that only works because another layer compensates is
   rejected.
3. Abstraction is expensive. One trait per real concept, no speculative generality.
4. Performance matters where performance matters. Hot paths: zero allocation per request
   after warm-up (body buffers from a pool only); no `String`, no `format!`, no `to_string()`;
   `Bytes` and `&[u8]` only; no async-fn boxing; no `Arc`/`Mutex` — per-worker state only.
5. Confidence must be earned. Every milestone passes the quality gate before being declared
   done. "It compiles" and "it runs once" are not done.
6. Names are correctness. Identifiers reflect semantics, ownership, lifecycle, and failure
   behaviour. Bag-of-everything names (`process`, `handle`, `manage`, `helper`, `util`,
   `service`) are rejected.
7. Error handling preserves truth. No catch-all swallowing. Every failure path answers: what
   failed, recoverable?, retry-safe?, cleanup needed?, state still valid?
8. Concurrency is reasoned, not guessed. For shared state, document visibility, atomicity,
   ordering, cancellation races, and lifetime after publication.
9. Resource ownership is unambiguous. Every resource has a documented owner, closer, and
   invalidation rule.
10. One concern per commit. Cleanup is not bundled with behavioural change.

## Architecture

A six-layer stack. Each layer owns one concern; dependency direction is downward and is
enforced by `cargo deny check` via `[[bans.deny]]` entries in `deny.toml`. A pull request that
introduces an upward edge fails CI.

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

`conduit-proto` exposes the single shared request/response stream type that every protocol
implementation maps to and that the lifecycle layer consumes. It is the one explicit
abstraction the project pays for.

Cross-cutting concerns (tracing) flow through layers via explicit context, never thread-locals.

## Runtime model

**Default: monoio** — `io_uring`, thread-per-core, share-nothing, with `SO_REUSEPORT` on
listeners and one worker per core, pinned. No work stealing; uniform traffic does not need it
and atomics cost real cycles.

**Feature flag: `runtime-tokio`** — keeps a tokio backend behind a cargo feature for
portability builds and comparison benchmarks. Hot-path code compiles and runs on both backends
but is tuned for monoio.

Implications baked into every layer:

- No `tokio::spawn`, no `tokio::sync` primitives on the hot path.
- Per-worker state is `Rc<RefCell<…>>` or owned, never `Arc<Mutex<…>>`.
- Cross-worker communication via `SO_REUSEPORT` (kernel-side load balancing) or cold-path
  message passing only.

## Supported Platforms

Cross-references the runtime-model section: backend selection follows platform support.

### Production

Release artifacts are published only for Linux:

- `x86_64-unknown-linux-gnu` (glibc, most distributions)
- `x86_64-unknown-linux-musl` (static, Alpine)
- `aarch64-unknown-linux-gnu` (glibc, ARM64)
- `aarch64-unknown-linux-musl` (static, ARM64)
- Multi-arch Docker image (`linux/amd64`, `linux/arm64`), distroless final stage

### Development

Development is supported on macOS via the `runtime-tokio` feature, auto-selected at compile
time when `cfg(not(target_os = "linux"))`. CI builds, type-checks, lints, and runs portable
tests on macOS to prevent the cross-platform compile path from rotting. **No performance
guarantees apply to non-Linux builds.** Benchmark numbers in `BENCHMARKS.md` are scoped to the
Linux target workload exclusively.

### Not Supported

Windows is not a supported target. The build may or may not succeed; if it does, the result is
not tested and not recommended for any use.

## Quality gate

Every milestone passes all of these before being declared done:

1. `cargo build --workspace --all-targets --release`
2. `cargo test --workspace`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --all -- --check`
5. `cargo deny check`
6. Integration tests for the milestone's protocol(s) pass against real wire traffic.
7. For hot-path changes: a criterion benchmark in `bench/micro/` shows no regression versus
   the previous milestone (or commit message explains the win).
8. For concurrent code: at least one `loom` test covering the new synchronisation.
9. For parsers: a `cargo-fuzz` target exists and has run ≥ 1 minute clean.

A skipped test is a red flag and must be surfaced, not glossed.

## Reporting discipline

At the end of each phase the maintainer produces:

- the workspace tree
- what changed
- the gate output (verbatim final lines for each step)
- what is intentionally not yet covered
- any deviation from this charter and why

No phase is declared complete by saying "should be green". The status names what ran.
