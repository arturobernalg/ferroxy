# Comparison harness (conduit vs nginx vs Pingora)

This directory holds the harness for **system-level comparison runs**
between conduit, nginx, and Cloudflare Pingora. The harness lives
here because the charter requires a separate, methodologically-strict
bar for any number that says "vs nginx" out loud.

## Status

The harness scripts and config templates are checked in. The
**numbers are not** — they are produced only on a real benchmark
box that meets the methodology bar. Loopback runs are regression-
only and never published. See
[`BENCHMARKS.md`](../../BENCHMARKS.md) for the methodology and
[`feedback_no_loopback_comparison`](../../README.md) for the rule.

> If a future PR adds "vs nginx" numbers from a loopback run, that
> PR is wrong. Block it. The numbers will be either artificially
> good (no real congestion control on loopback) or bottlenecked by
> the test backend, and either way they'll haunt the project.

## What's here

| File                    | Purpose |
|-------------------------|---------|
| `compare.sh`            | Orchestrator. Runs each candidate (conduit / nginx / Pingora) for `$DURATION` seconds at `$CONNECTIONS` connections, captures wrk output and resource metrics, writes a CSV. |
| `nginx.conf.template`   | Pinned nginx config for the comparison run. No proxy_buffering, no caching, no logs. Render to `nginx.conf` with environment substitution. |
| `pingora.toml.template` | Pinned Pingora config (when added). |
| `sysctls.sh`            | Kernel sysctls applied before the run. Idempotent. Must be run as root. |
| `methodology.md`        | The exact methodology this harness implements. Read it before claiming a number. |

## What's not here

- **Pre-baked nginx / Pingora numbers.** Those come from a real run on
  real hardware on a real day, and they are recorded in
  `BENCHMARKS.md` once produced — not committed alongside the harness.
- **A "single-machine quick run"** mode. We deliberately do not
  expose one; the regression harness in `bench/run.sh` is for
  conduit-only loopback regressions, not comparison.

## Required environment

The harness refuses to run unless every check passes:

1. The kernel is ≥ 6.6 (`uname -r`).
2. The NIC named in `$BENCH_NIC` is up and not loopback.
3. `wrk`, `nginx`, and the conduit / Pingora binaries are pinned to
   specific versions recorded in `methodology.md`.
4. `cpufreq-info` reports either Turbo Boost off or the current
   max frequency is recorded for the result file.
5. If multi-socket, `$BENCH_NUMA_LOAD` is set to a NUMA node
   different from `$BENCH_NUMA_PROXY`.

## Running it

On a real bench box, after `sysctls.sh` and the env checks:

```bash
DURATION=120 CONNECTIONS=512 REPEATS=5 ./compare.sh > result-$(date +%F).csv
```

The CSV columns are: `candidate, run_idx, rps, p50_ms, p95_ms,
p99_ms, p999_ms, cpu_percent, rss_mb, syscalls_per_req`. The
methodology block goes alongside the CSV in the same directory; do
not hand someone the CSV without it.

## Until a real box exists

Phase 11 of the charter is **gated** on a real bench box. The
scripts here are infrastructure prepared in advance, not something
to run on a laptop. If you don't have access to a 16-core box with
a real NIC, kernel ≥ 6.6, and pinned sysctls, the right answer to
"are we faster than nginx?" is "architecturally plausible,
empirically unproven" — that's a **stronger** position than a
misleading benchmark.
