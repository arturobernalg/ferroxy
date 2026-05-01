# Comparison methodology

Every number in the conduit-vs-nginx-vs-Pingora result file must
satisfy every bullet below. A misleading benchmark is a **worse**
outcome than no benchmark — readers cite numbers regardless of the
disclaimer next to them.

## Hardware

- 16 cores minimum (the charter's target workload).
- Real NIC, ≥ 10 Gbps. **Loopback is forbidden** for comparison runs.
- Kernel ≥ 6.6.
- If the box has > 1 NUMA node: wrk pinned to a different node than
  the proxies.
- Turbo Boost: either disabled (preferred) or the current max
  frequency is recorded in the result file.

## Kernel sysctls

Apply via `sysctls.sh` as root. The exact values applied for the
run go in the result file. Defaults across distros move numbers
± 20%; pinning is non-negotiable.

```sh
net.core.somaxconn = 65535
net.core.netdev_max_backlog = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 15
net.ipv4.tcp_keepalive_time = 60
net.ipv4.ip_local_port_range = 1024 65535
fs.file-max = 1048576
```

## Versions

Pinned versions go in the result file alongside the numbers.
Approximate set:

- `conduit`: the SHA at run time
- `nginx`: latest stable on the day of the run
- `pingora`: latest tagged release on the day of the run
- `wrk`: pin the version

## Run shape

- Each scenario runs **≥ 60 s**. Warm-up should not dominate.
- **≥ 3 repeats** per scenario (recommend 5 for stable spread).
- Report median + min + max. Never the single best run.
- Identical wrk parameters across all candidates.
- One body shape per scenario (do not average across shapes):
  - small: GET → 1 KB response
  - medium: GET → 4 KB response (the default)
  - large: GET → 64 KB response

## What gets captured

Per run, into the CSV:

- `rps` — requests per second
- `p50_ms`, `p95_ms`, `p99_ms`, `p999_ms` — wrk's `--latency` output
- `cpu_percent` — peak CPU %, measured via `pidstat` against the proxy
- `rss_mb` — peak resident set size
- `syscalls_per_req` — captured via `perf stat -e raw_syscalls:sys_enter`
  during a 30 s subset

## What gets disabled

The proxies must be configured to do nothing the comparison doesn't
isolate:

- nginx: no `proxy_buffering`, no caching, no access log, `worker_connections`
  set high enough not to be a knob.
- conduit: no metrics scrape during the run (the admin server stays up
  but is not loaded).
- Pingora: defaults except for the analogous knobs above.

## Reporting

The result file goes alongside the CSV in
`bench/compare/results/YYYY-MM-DD-<box>-<scenario>.md` with this
shape:

```markdown
# Compare run YYYY-MM-DD

## Hardware
...

## Sysctls
...

## Versions
- conduit: <sha>
- nginx:   <version>
- pingora: <version>
- wrk:     <version>

## Scenario
- body: 4 KB
- duration: 120s
- connections: 512
- threads: 16
- repeats: 5

## Results

| candidate | rps (median) | p50 ms | p99 ms | rps (min) | rps (max) |
| --------- | ------------ | ------ | ------ | --------- | --------- |
| ...

## Caveats
...
```
