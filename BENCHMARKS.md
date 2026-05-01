# Benchmarks

This document describes how conduit is benchmarked, what numbers we
target, and how to reproduce a run.

## Target workload

The engineering charter specifies the following box and load:

- **Hardware:** Linux ≥ 6.6, 16 cores, 25 Gbps NIC, 64 GiB RAM
- **Concurrency:** 10 000 concurrent connections
- **Throughput:** sustained 500 000 requests/second
- **Latency budget:** p50 < 1 ms, p99 < 5 ms (proxy overhead, not
  end-to-end)
- **Body size:** request <1 KB p95, response <8 KB p95

These numbers are aspirational targets for the production build on
production hardware. The harness in `bench/run.sh` ships with the
loopback / single-box version of the same shape so contributors can
catch regressions on their laptops.

## Local repro

`bench/run.sh` orchestrates a self-contained run:

```
wrk  --(127.0.0.1:8000)-->  conduit  --(127.0.0.1:8001)-->  python http.server
```

Required tools: `wrk`, `python3`, `cargo`. `oha` works as a drop-in if
you `export LOADGEN=oha`.

```bash
./bench/run.sh
DURATION=60 CONNECTIONS=512 THREADS=8 ./bench/run.sh
```

The script:
1. Builds conduit in release mode.
2. Starts the static-file backend on 127.0.0.1:8001.
3. Starts conduit on 127.0.0.1:8000 with `bench/conduit.toml`.
4. Waits for both ports to come up, then smoke-tests with `curl`.
5. Runs `wrk` for `$DURATION`, with `$CONNECTIONS` connections across
   `$THREADS` threads.
6. Prints the conduit `/metrics` snapshot at the end.
7. Cleans up both processes on exit (trap'd).

The numbers from the loopback run are **not** the charter targets —
they are bounded by the python backend's throughput. Use the run as a
*regression detector* when changing hot paths.

## Build profiles

Two release profiles ship in the workspace `Cargo.toml`:

- **`release`** — opt-level=3, thin LTO, codegen-units=1, line tables.
  Default for `cargo build --release` and CI. Build time ~30 s on
  a typical box.
- **`release-bench`** — same as `release` plus **fat LTO** and
  symbol stripping. ~2-3× the build time (60-90 s); enables
  cross-crate inlining of hyper internals into our handlers, which
  is where most of the compile-time runtime payoff lives.

Use `release-bench` for:
- The published artefact (release.yml's binary build).
- Any comparison run against nginx / Pingora.
- Profile-driven optimisation (the profile data should reflect the
  optimised binary, not the dev build).

```bash
cargo build --profile release-bench --bin conduit
cargo bench --profile release-bench
```

Microbench numbers under `release-bench` show small gains on the
routing slice (~1-4%); the larger gains land on end-to-end RPS
once bench infrastructure exists.

## Microbenchmarks

Per-component criterion benches live next to their owning crate
(`crates/<crate>/benches/<name>.rs`) and are indexed in
[`bench/micro/README.md`](./bench/micro/README.md). A microbench
number is publishable on its own merits because the methodology is
bounded — `cargo bench`, release profile, `black_box` on inputs. The
current set:

- `route_lookup` (conduit-lifecycle): 32–50 ns per lookup at
  100 hosts × 10 prefixes. Documented baseline lives in
  [`bench/micro/README.md`](./bench/micro/README.md).

When a hot-path change lands, the bench gets rerun and the baseline
in that file is updated alongside the commit. A regression that
isn't justified in the commit message is a CI gate failure (charter
quality-gate item 7).

## Comparison runs

Comparison runs against nginx and Pingora live in
[`bench/compare/`](./bench/compare/) with a methodology bar so
strict that the harness refuses to start if it isn't met. The
methodology is captured in
[`bench/compare/methodology.md`](./bench/compare/methodology.md).
Summary:

- Real NIC. **Loopback is forbidden** for comparison runs and the
  harness refuses `BENCH_NIC=lo`.
- Kernel ≥ 6.6, sysctls applied via `bench/compare/sysctls.sh`.
- Pinned versions of nginx, Pingora, conduit, wrk — the SHAs / tags
  go in the result file.
- ≥ 60 s per scenario, ≥ 3 repeats (5 recommended), report median +
  spread.
- Multi-socket: wrk pinned to a different NUMA node than the
  proxies.
- Fixed body shape per scenario (1 KB / 4 KB / 64 KB), one number
  per shape; never averaged.

We refuse to publish "vs nginx" numbers from loopback runs — if you
see them in the repo, treat them as a bug. The charter is explicit
on this; the user has been explicit on this; the harness enforces
it. Until a real bench box exists, the honest status is
"architecturally plausible, empirically unproven" — that is a
**stronger** position than a misleading benchmark.

## Profiling

Charter rule for P11.5: top-5 hot functions must be identified by
profiling on the bench workload, not by reading the code. Workflow:

```bash
RUSTFLAGS='-C force-frame-pointers=yes' cargo build --release
perf record -F 999 -g --call-graph dwarf -- ./target/release/conduit \
    --config bench/conduit.toml &
# in another terminal: run bench/run.sh
perf report > bench/.profile.txt
```

`flamegraph` (`cargo install flamegraph`) is the more readable
alternative once you've identified an interesting region.

## Optimisation gate (P11.6)

The charter explicitly forbids replacing hyper on the hot path **unless
profiling justifies it**. The replacement is a P11.6 deliverable
conditional on the P11.5 profile showing hyper as a top-3 hot function
that we cannot tune around. Until that profile exists, hyper stays.
