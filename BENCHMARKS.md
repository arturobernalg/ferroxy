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

## Comparison runs (P11.x)

Comparing against nginx and Pingora is part of the charter and lives
in P11.x. The comparison plan:

- Fix a single body size (4 KB response) and request shape (`GET /static-file`).
- Use **identical** wrk parameters for all three.
- Pin versions: nginx = latest stable, Pingora = latest tagged release.
- Ensure no side-channel I/O (no logging to disk, no proxy_buffering on
  nginx, no caching).
- Run on a real NIC, not loopback. Loopback short-circuits TCP and
  paints a misleading picture of congestion control.
- Capture: RPS, p50, p95, p99, p99.9, CPU%, RSS, syscalls/req
  (perf trace).

The comparison harness will land in `bench/compare.sh` once a real
test bench is wired up. We refuse to publish "vs nginx" numbers from
loopback runs — if you see them in the repo, treat them as a bug.

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
