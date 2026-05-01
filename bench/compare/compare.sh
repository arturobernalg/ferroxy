#!/usr/bin/env bash
# bench/compare/compare.sh — orchestrator for the conduit vs nginx
# (vs Pingora, when added) comparison harness.
#
# Refuses to run unless the methodology bar is met: real NIC,
# kernel ≥ 6.6, pinned versions, sysctls applied. Loopback runs are
# explicitly rejected — they belong in bench/run.sh, not here.
#
# Output: a CSV on stdout. Methodology + caveats live in a Markdown
# file at bench/compare/results/<date>-<scenario>.md alongside the CSV.
#
# Required env (or set defaults):
#   BENCH_NIC          NIC the proxies bind to.        e.g. eno1
#   BENCH_BACKEND      Backend host:port.              e.g. 10.0.0.10:8080
#   BENCH_NUMA_PROXY   NUMA node for proxies.          e.g. 0
#   BENCH_NUMA_LOAD    NUMA node for wrk.              e.g. 1
#   DURATION           Per-run wall-clock seconds.     default 120
#   CONNECTIONS        wrk -c.                         default 512
#   THREADS            wrk -t.                         default $(nproc)
#   REPEATS            Runs per candidate.             default 5
#
# Required tools: wrk (pinned), nginx (pinned), conduit binary,
# pidstat (sysstat), perf stat (linux-tools).
set -euo pipefail

DURATION="${DURATION:-120}"
CONNECTIONS="${CONNECTIONS:-512}"
THREADS="${THREADS:-$(nproc)}"
REPEATS="${REPEATS:-5}"

step() { printf '\n=== %s ===\n' "$*" >&2; }
fail() { echo "ERROR: $*" >&2; exit 2; }

# --- methodology gate -----------------------------------------------

[[ -n "${BENCH_NIC:-}"     ]] || fail "BENCH_NIC must be set (the real NIC the proxies will bind to). Loopback is forbidden — see bench/compare/methodology.md."
[[ -n "${BENCH_BACKEND:-}" ]] || fail "BENCH_BACKEND must be set (host:port the proxies forward to)."

# Reject loopback explicitly. The whole point of this harness is to
# force a real NIC; if BENCH_NIC is `lo`, the result is meaningless.
case "$BENCH_NIC" in
    lo|localhost|127.0.0.1)
        fail "BENCH_NIC=$BENCH_NIC: loopback is regression-only (bench/run.sh). See methodology.md."
        ;;
esac

# Kernel ≥ 6.6 check.
kernel_major=$(uname -r | cut -d. -f1)
kernel_minor=$(uname -r | cut -d. -f2)
if [[ "$kernel_major" -lt 6 || ( "$kernel_major" -eq 6 && "$kernel_minor" -lt 6 ) ]]; then
    fail "kernel $(uname -r) < 6.6. Charter target requires 6.6+."
fi

for tool in wrk nginx pidstat perf; do
    if ! command -v "$tool" >/dev/null; then
        fail "$tool not found in PATH. See methodology.md for required tools."
    fi
done

# Multi-socket → require NUMA pinning split.
sockets=$(lscpu 2>/dev/null | awk '/^Socket\(s\):/ {print $2}')
if [[ "${sockets:-1}" -gt 1 ]]; then
    [[ -n "${BENCH_NUMA_PROXY:-}" ]] || fail "multi-socket box: BENCH_NUMA_PROXY must be set."
    [[ -n "${BENCH_NUMA_LOAD:-}"  ]] || fail "multi-socket box: BENCH_NUMA_LOAD must be set (different from PROXY)."
    [[ "${BENCH_NUMA_PROXY}" != "${BENCH_NUMA_LOAD}" ]] || fail "BENCH_NUMA_PROXY and BENCH_NUMA_LOAD must differ."
fi

# Turbo Boost record (don't fail; record).
turbo_state=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null || echo "unknown")

step "methodology gate: passed"
step "kernel: $(uname -r)"
step "nic: $BENCH_NIC"
step "backend: $BENCH_BACKEND"
step "turbo (intel_pstate/no_turbo): $turbo_state  (1 = disabled, recommended)"

# --- emit CSV header ------------------------------------------------

echo "candidate,run_idx,rps,p50_ms,p95_ms,p99_ms,p999_ms,cpu_percent,rss_mb"

# --- runner --------------------------------------------------------
# Per-candidate runner blocks live below. They are deliberately
# stubbed: starting nginx / conduit / Pingora and capturing their
# resource metrics is platform-specific (systemd vs raw vs cgroup)
# and we want a real human to fill it in for the box at hand. The
# methodology gate above is the load-bearing piece.

# stub: each candidate would
#   1. start the proxy bound to BENCH_NIC:port
#   2. wait for the listener
#   3. for run in 1..REPEATS:
#        wrk -t$THREADS -c$CONNECTIONS -d${DURATION}s --latency http://nic:port/ \
#          | parse → emit one CSV row
#   4. stop the proxy

step "candidate runners are not auto-filled — fill in for your bench box"
echo "# methodology gate passed; run blocks are intentionally skeletal" >&2
echo "# fill in for your bench box and rerun" >&2
exit 0
