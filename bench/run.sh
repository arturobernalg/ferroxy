#!/usr/bin/env bash
# bench/run.sh — repro harness for measuring conduit's plaintext H1
# throughput on a single box.
#
# Layout:
#
#   wrk  --(:8000)-->  conduit  --(:8001)-->  backend (python http.server)
#
# Every component runs on loopback. The harness:
#   1. builds conduit in release mode (cached on subsequent runs)
#   2. starts a static-file backend on :8001
#   3. starts conduit on :8000 with bench/conduit.toml
#   4. waits for both ports to come up
#   5. runs `wrk` for $DURATION seconds at $CONNECTIONS connections
#   6. tears everything down
#
# Required:  wrk, python3, cargo
# Optional:  oha (alternative load generator; export LOADGEN=oha)
#
# Override defaults by exporting:
#   DURATION=30 CONNECTIONS=256 THREADS=4 ./bench/run.sh
#
# nginx / Pingora comparison runs are P11.x scope; they need a stable
# version pin and identical body sizes. See BENCHMARKS.md for plan.
set -euo pipefail

# Resolve repo root regardless of where the script is invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

DURATION="${DURATION:-30}"
CONNECTIONS="${CONNECTIONS:-256}"
THREADS="${THREADS:-$(nproc 2>/dev/null || echo 4)}"
LOADGEN="${LOADGEN:-wrk}"

CONDUIT_PORT=8000
BACKEND_PORT=8001
ADMIN_PORT=9000

step() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
    set +e
    [[ -n "${CONDUIT_PID:-}" ]] && kill "$CONDUIT_PID" 2>/dev/null
    [[ -n "${BACKEND_PID:-}" ]] && kill "$BACKEND_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

step "build conduit (release)"
cargo build --release --bin conduit

step "start backend (python http.server) on :$BACKEND_PORT"
mkdir -p bench/.fixtures
# Single small file so wrk gets a deterministic body.
printf 'hello from bench backend\n' > bench/.fixtures/index.txt
( cd bench/.fixtures && python3 -m http.server "$BACKEND_PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
BACKEND_PID=$!

step "start conduit on :$CONDUIT_PORT"
RUST_LOG=warn ./target/release/conduit --config bench/conduit.toml &
CONDUIT_PID=$!

step "wait for ports"
for port in "$BACKEND_PORT" "$CONDUIT_PORT" "$ADMIN_PORT"; do
    for _ in $(seq 1 50); do
        if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then break; fi
        sleep 0.1
    done
done

step "smoke test"
curl -fsS "http://127.0.0.1:$CONDUIT_PORT/index.txt" >/dev/null
echo "  smoke ok"

step "load: $LOADGEN  duration=${DURATION}s  conns=$CONNECTIONS  threads=$THREADS"
case "$LOADGEN" in
    wrk)
        wrk -t "$THREADS" -c "$CONNECTIONS" -d "${DURATION}s" --latency \
            "http://127.0.0.1:$CONDUIT_PORT/index.txt"
        ;;
    oha)
        oha -c "$CONNECTIONS" -z "${DURATION}s" --no-tui \
            "http://127.0.0.1:$CONDUIT_PORT/index.txt"
        ;;
    *)
        echo "unknown LOADGEN=$LOADGEN" >&2
        exit 2
        ;;
esac

step "admin /metrics snapshot"
curl -fsS "http://127.0.0.1:$ADMIN_PORT/metrics" || true
