# Configuration reference

conduit reads a single TOML file at the path supplied via `--config`.
Every key documented here is parsed by `conduit-config`; unknown keys
fail validation with the offending key surfaced.

A complete example lives in [`examples/conduit.toml`](examples/conduit.toml).

```bash
conduit --config /etc/conduit/conduit.toml
conduit --config /etc/conduit/conduit.toml --check    # validate-only, exit 0 / 2
```

## Top-level layout

```toml
[server]   # listeners, runtime, admin
[tls]      # certs (HTTPS / H3) — optional
[[upstream]] # one block per upstream pool
[[route]]    # one block per routing rule
```

## `[server]`

| Key              | Type            | Default      | Notes |
|------------------|-----------------|--------------|-------|
| `runtime`        | `"monoio"` / `"tokio"` | `"tokio"` (dev), `"monoio"` (Linux release) | Selected at compile time today; runtime selection lands when the monoio↔tokio bridge ships. |
| `workers`        | `"auto"` / integer | `"auto"`     | `"auto"` resolves to `available_parallelism()`. |
| `cpu_affinity`   | bool            | `false`      | Pin worker threads to cores. monoio only. |
| `listen_http`    | array of `host:port` | `[]`     | Plaintext HTTP/1.1 listeners. |
| `listen_https`   | array of `host:port` | `[]`     | TLS listeners (HTTP/1.1 + HTTP/2 via ALPN). |
| `listen_h3`      | array of `host:port` | `[]`     | QUIC listeners (HTTP/3). |
| `admin_listen`   | `host:port`     | `127.0.0.1:9090` | Admin server (`/health`, `/ready`, `/metrics`). |
| `metrics_listen` | `host:port`     | optional     | Reserved for a future split between admin and metrics; not used today. |

The binary refuses to start if all three `listen_*` arrays are empty
(exit 78).

## `[tls]`

Required if any `listen_https` or `listen_h3` entry is set.

| Key              | Type     | Default | Notes |
|------------------|----------|---------|-------|
| `min_version`    | `"1.2"` / `"1.3"` | `"1.2"` | TLS 1.3 is mandatory for QUIC regardless. |
| `alpn`           | array    | `["h2", "http/1.1"]` for HTTPS; `["h3"]` for QUIC | Today the binary overrides QUIC's ALPN to `["h3"]`. |
| `session_tickets`| bool     | `true`  | rustls default. |
| `ocsp_stapling`  | bool     | `true`  | Reserved; OCSP stapling is P6.x scope. |
| `certs`          | array of `{sni, cert, key}` | required | First entry is used for HTTPS today; SNI selection is P6.x scope. |

## `[[upstream]]`

```toml
[[upstream]]
name  = "api"
addrs = ["10.0.0.1:8080", "10.0.0.2:8080"]
protocol = "http1"
lb = "round_robin"
connect_timeout = "2s"
pool = { max_idle_per_host = 64, idle_timeout = "60s", max_lifetime = "10m" }
health_check = { path = "/healthz", interval = "5s", unhealthy_threshold = 3, timeout = "1s" }
retry = { attempts = 2, on_status = [502, 503, 504], on_connect_error = true }
```

| Key             | Type | Notes |
|-----------------|------|-------|
| `name`          | string | Referenced from `[[route]]`. |
| `addrs`         | array of `host:port` | Round-robin'd; per-addr passive breaker rotates a bad backend out without taking down the pool. |
| `protocol`      | `"http1"` / `"h2"` | `"h2"` upstream is P7.x scope; `"http1"` is the only working value. |
| `lb`            | `"round_robin"` / `"p2c"` | Only `"round_robin"` ships today. |
| `connect_timeout` | duration | Configures hyper-util's `HttpConnector::set_connect_timeout`. |
| `pool`          | table | `max_idle_per_host` / `idle_timeout` / `max_lifetime`. |
| `health_check`  | table | Active probes (see below). Updates the same per-addr breakers the request hot path consults. |
| `retry`         | table | `attempts`, `on_status` (list of HTTP codes), `on_connect_error` (bool). |

### `[[upstream]] health_check`

| Key                   | Type     | Notes |
|-----------------------|----------|-------|
| `path`                | string   | GET path to probe. Defaults to `/healthz`. |
| `interval`            | duration | Sleep between probes per addr. |
| `timeout`             | duration | Per-probe deadline; counts as a failure if the response doesn't arrive. |
| `unhealthy_threshold` | integer  | Consecutive failures that flip the addr's breaker to Open. |
| `healthy_threshold`   | integer  | Consecutive successes that flip the breaker back to Closed. |

## `[[route]]`

```toml
[[route]]
match = { host = "api.example.com", path_prefix = "/" }
upstream = "api"
timeouts = { connect = "2s", read = "30s", write = "30s", total = "60s" }
retry = { attempts = 2, on_status = [502, 503, 504], on_connect_error = true }
```

| Key       | Type    | Notes |
|-----------|---------|-------|
| `match.host` | glob (e.g. `*.example.com`) | `*` matches any. |
| `match.path_prefix` | string | Indexed by per-host bucket: O(1) host hash + O(prefixes) longest-prefix scan. |
| `match.path_exact` | string | O(1) hash lookup; beats prefix in the same bucket. |
| `match.path_regex` | string | Compiled at config-load. Tried after exact + prefix in declaration order; first match wins. |
| `upstream` | string  | Must match an `[[upstream]] name`. |
| `timeouts.total` | duration | Wall-clock cap on the entire request; `tokio::time::timeout` returns 504 on expiry. |
| `timeouts.connect` / `read` / `write` | duration | `connect` is wired through to the upstream's `HttpConnector`; `read` / `write` per-leg timeouts still need a hyper-util config rebuild and are next P5.x. |
| `retry`    | table   | Same shape as upstream-level retry. Per-route retry overrides the upstream's. |

### `[[route]] match.headers`

```toml
[[route]]
match = { host = "api.example.com", headers = [{ name = "x-tier", value = "gold" }] }
upstream = "api-gold"
```

Header-based matching is parsed but not yet evaluated against
requests; routes with header predicates fall back to plain host+path
matching today. Header eval lands when the route bucket grows
multi-route-per-slot support (P5.x).

## Hot-reload

`SIGHUP` re-reads the config from the same path the binary was started
with. Supported changes:

- `[[route]]` adds / removes / re-orders / `upstream` rewires
- `[[upstream]] addrs` set rewrite (the pool itself rebuilds)
- `[[upstream]] retry`, `connect_timeout` / route-level `retry`, `timeouts.total`
- Path matchers: `path_exact` / `path_prefix` / `path_regex` swaps

**Not** hot-reloadable today — restart the process:

- `[server] listen_*`, `admin_listen`
- `[tls] certs` / `min_version`
- `[server] workers` / `runtime` / `cpu_affinity`
- `[upstream] health_check` (probe tasks bind config at startup)

A reload that fails validation logs the error and leaves the previous
configuration running. The proxy never ends up in a half-loaded state.

## Admin endpoints

The admin server runs on `[server] admin_listen` (default
`127.0.0.1:9090`). It speaks HTTP/1.1 and exposes:

| Path         | Notes |
|--------------|-------|
| `GET /health`     | Liveness — `200 OK` while the process is up. |
| `GET /ready`      | Readiness — currently always `200 OK` (startup is synchronous). Will track upstream health when active health checks ship in P4.x. |
| `GET /metrics`    | Prometheus 0.0.4 text exposition: uptime gauge, build_info gauge, request counters by outcome / status class, request-duration histogram (Prometheus default buckets, 1ms..10s). |
| `GET /upstreams`  | One line per upstream: name, addr list, breaker state (`closed` / `open` / `half_open`). |
| `GET /`           | Lists endpoints. |
