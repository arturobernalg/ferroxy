//! Configuration schema: the typed shape of `conduit.toml`.
//!
//! Every record uses `#[serde(deny_unknown_fields)]`, with one explicit
//! exception ([`Filter`]) that flattens filter-specific parameters. Unknown
//! keys are a hard error so that misspelled config never silently degrades
//! into default behaviour.
//!
//! This file is the single source of truth for the configuration surface.
//! Validation that crosses fields lives in `validate.rs`; everything here
//! is local-to-one-record.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use crate::duration::DurationStr;
use crate::workers::Workers;

/// Top-level configuration document.
///
/// Construct with [`crate::parse`] or [`crate::load`]; do not call
/// `Config::deserialize` directly — that bypasses semantic validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Server / runtime / listener configuration.
    pub server: ServerConfig,

    /// TLS termination, optional. Required if `server.listen_https` or
    /// `server.listen_h3` is non-empty.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Upstream pools that routes target by name.
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<Upstream>,

    /// Routing rules evaluated in declared order.
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}

// ---------------------------------------------------------------------------
// [server]
// ---------------------------------------------------------------------------

/// Server-wide settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Runtime backend.
    #[serde(default)]
    pub runtime: RuntimeMode,

    /// Worker count. `"auto"` picks at startup from available cores.
    #[serde(default)]
    pub workers: Workers,

    /// Pin worker N to core N. Ignored on platforms without affinity.
    #[serde(default = "true_")]
    pub cpu_affinity: bool,

    /// Plain HTTP listeners.
    #[serde(default)]
    pub listen_http: Vec<SocketAddr>,

    /// HTTPS (TCP+TLS) listeners.
    #[serde(default)]
    pub listen_https: Vec<SocketAddr>,

    /// HTTP/3 (UDP+QUIC) listeners.
    #[serde(default)]
    pub listen_h3: Vec<SocketAddr>,

    /// Admin endpoints listener (`/health`, `/ready`, …).
    pub admin_listen: SocketAddr,

    /// Prometheus `/metrics` listener.
    pub metrics_listen: SocketAddr,
}

/// Runtime backend.
///
/// `Monoio` is the default and the target for the win-condition workload:
/// `io_uring`, thread-per-core, share-nothing, no work-stealing. `Tokio` is
/// kept behind a binary cargo feature for portability and comparison
/// benchmarks; selecting it requires the binary to be built with
/// `--features runtime-tokio` (will land in phase 1 alongside the monoio
/// implementation).
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// monoio + `io_uring` + thread-per-core. Default.
    #[default]
    Monoio,
    /// tokio multi-thread runtime. Comparison/portability only.
    Tokio,
}

// ---------------------------------------------------------------------------
// [tls]
// ---------------------------------------------------------------------------

/// TLS termination configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// One certificate per SNI hostname (or `*.host` wildcard).
    pub certs: Vec<CertSpec>,

    /// Minimum negotiated TLS version. Default: 1.2.
    #[serde(default)]
    pub min_version: TlsMinVersion,

    /// ALPN protocol identifiers offered to clients, in preference order.
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,

    /// Issue session resumption tickets. Default: true.
    #[serde(default = "true_")]
    pub session_tickets: bool,

    /// Staple OCSP responses. Default: true.
    #[serde(default = "true_")]
    pub ocsp_stapling: bool,

    /// Allow QUIC 0-RTT early data on the H3 listener. Default:
    /// false. **Security note**: 0-RTT data is replayable by a
    /// network attacker and must only be used for idempotent
    /// requests. Charter rule: off unless the operator opts in.
    #[serde(default)]
    pub enable_0rtt: bool,
}

/// Minimum negotiated TLS version.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMinVersion {
    /// TLS 1.2.
    #[default]
    #[serde(rename = "1.2")]
    V1_2,
    /// TLS 1.3.
    #[serde(rename = "1.3")]
    V1_3,
}

/// One SNI hostname → cert + key pair.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertSpec {
    /// SNI hostname; may begin with `*.` to indicate wildcard.
    pub sni: String,
    /// PEM-encoded certificate chain.
    pub cert: PathBuf,
    /// PEM-encoded private key.
    pub key: PathBuf,
    /// Optional pre-fetched OCSP response (DER) attached to the
    /// `CertifiedKey`. Cert hot-reload picks the new file up too.
    /// Operators are expected to refresh this file out-of-band
    /// (typically via certbot's `--deploy-hook` running
    /// `openssl ocsp ... > $path` and then `kill -HUP $pid`).
    #[serde(default)]
    pub ocsp_response: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// [[upstream]]
// ---------------------------------------------------------------------------

/// One upstream cluster, addressable by `name` from `[[route]]` blocks.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Cluster name. Must be unique across the configuration.
    pub name: String,

    /// Backend addresses. At least one is required (enforced in validation).
    pub addrs: Vec<SocketAddr>,

    /// Wire protocol used to talk to the backend.
    pub protocol: UpstreamProtocol,

    /// TLS for the upstream side; absent means plaintext to the backend.
    #[serde(default)]
    pub tls: Option<UpstreamTls>,

    /// Load-balancing algorithm.
    #[serde(default)]
    pub lb: LoadBalancer,

    /// Connection-pool limits.
    #[serde(default)]
    pub pool: PoolLimits,

    /// Connect timeout to a single backend address.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: DurationStr,

    /// Active health-check policy. Absent means passive-only ejection
    /// (see circuit breaker, when implemented).
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
}

/// Protocol used to speak to an upstream.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProtocol {
    /// HTTP/1.1.
    Http1,
    /// HTTP/2.
    H2,
}

/// Load-balancing algorithm for an upstream cluster.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancer {
    /// Round-robin over healthy backends.
    #[default]
    RoundRobin,
    /// Pick the backend with the fewest in-flight requests.
    LeastConn,
    /// Power-of-two-choices: random pair, pick the lighter.
    P2c,
    /// Consistent hash over the request key (cache affinity).
    ConsistentHash,
}

/// TLS parameters for the upstream side of a connection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTls {
    /// SNI to send to the backend. Defaults to the address host.
    #[serde(default)]
    pub server_name: Option<String>,
    /// CA bundle for verifying the backend cert. Defaults to platform roots.
    #[serde(default)]
    pub ca: Option<PathBuf>,
    /// Client certificate (mTLS).
    #[serde(default)]
    pub client_cert: Option<PathBuf>,
    /// Client key (mTLS).
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    /// Verify the backend cert against the CA bundle. Default: true.
    #[serde(default = "true_")]
    pub verify: bool,
}

/// Limits on the per-(authority, protocol, tls-params) connection pool.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct PoolLimits {
    /// Maximum idle connections kept per backend host.
    #[serde(default = "default_pool_max_idle")]
    pub max_idle_per_host: u32,
    /// Idle connections are closed after this duration.
    #[serde(default = "default_pool_idle_timeout")]
    pub idle_timeout: DurationStr,
    /// Connections older than this are closed even if still in use after
    /// their next request returns.
    #[serde(default = "default_pool_max_lifetime")]
    pub max_lifetime: DurationStr,
}

impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            max_idle_per_host: default_pool_max_idle(),
            idle_timeout: default_pool_idle_timeout(),
            max_lifetime: default_pool_max_lifetime(),
        }
    }
}

/// Active health-check policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// Path probed via GET.
    #[serde(default = "default_hc_path")]
    pub path: String,
    /// Probe interval.
    pub interval: DurationStr,
    /// Per-probe timeout.
    #[serde(default = "default_hc_timeout")]
    pub timeout: DurationStr,
    /// Consecutive probe failures that flip a backend to unhealthy.
    #[serde(default = "default_hc_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    /// Consecutive probe successes that flip a backend back to healthy.
    #[serde(default = "default_hc_healthy_threshold")]
    pub healthy_threshold: u32,
}

// ---------------------------------------------------------------------------
// [[route]]
// ---------------------------------------------------------------------------

/// One routing rule.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Match predicate. At least one of host / path_* must be set.
    #[serde(rename = "match")]
    pub match_: RouteMatch,

    /// Name of the upstream cluster this route forwards to.
    pub upstream: String,

    /// Per-route timeouts.
    #[serde(default)]
    pub timeouts: Timeouts,

    /// Retry policy. Absent means no retries.
    #[serde(default)]
    pub retry: Option<RetryPolicy>,

    /// Filter chain applied to matching requests. Filter semantics are
    /// implemented by Layer 4 (lifecycle); this crate parses them into
    /// inert records.
    #[serde(default)]
    pub filters: Vec<Filter>,
}

/// Predicate matched against the incoming request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// Match if `Host` header (or `:authority`) equals this value.
    #[serde(default)]
    pub host: Option<String>,
    /// Match if request path begins with this prefix.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Match if request path equals this value exactly.
    #[serde(default)]
    pub path_exact: Option<String>,
    /// Match if request path matches this regex. Compiled at config load.
    #[serde(default)]
    pub path_regex: Option<String>,
    /// All listed headers must match.
    #[serde(default)]
    pub headers: Vec<HeaderMatch>,
}

/// Single header-match predicate.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderMatch {
    /// Header name (case-insensitive comparison at runtime).
    pub name: String,
    /// Required header value (exact match).
    pub value: String,
}

/// Per-route timeout policy.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// TCP/QUIC connect to the backend.
    #[serde(default = "default_timeout_connect")]
    pub connect: DurationStr,
    /// Maximum gap between bytes read from the backend.
    #[serde(default = "default_timeout_read")]
    pub read: DurationStr,
    /// Maximum gap between bytes written to the backend.
    #[serde(default = "default_timeout_write")]
    pub write: DurationStr,
    /// Wall-clock cap on the entire request.
    #[serde(default = "default_timeout_total")]
    pub total: DurationStr,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: default_timeout_connect(),
            read: default_timeout_read(),
            write: default_timeout_write(),
            total: default_timeout_total(),
        }
    }
}

/// Retry policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Maximum total attempts (including the original try). Must be ≥ 1.
    pub attempts: u32,
    /// Status codes from the backend that trigger a retry.
    #[serde(default)]
    pub on_status: Vec<u16>,
    /// Retry on TCP/TLS connect failure.
    #[serde(default)]
    pub on_connect_error: bool,
}

/// One filter in a route's filter chain.
///
/// Filter `kind` and parameters are interpreted by Layer 4 (lifecycle).
/// This crate is intentionally permissive about the shape of `params` so
/// that adding new filters does not require touching the schema crate.
#[derive(Debug, Deserialize)]
pub struct Filter {
    /// Filter kind, e.g. `"strip-prefix"`, `"add-header"`.
    pub kind: String,
    /// Filter-specific parameters; the lifecycle layer validates these
    /// when the filter is instantiated.
    #[serde(flatten)]
    pub params: toml::value::Table,
}

// ---------------------------------------------------------------------------
// defaults
// ---------------------------------------------------------------------------

fn default_alpn() -> Vec<String> {
    vec!["h3".into(), "h2".into(), "http/1.1".into()]
}

const fn true_() -> bool {
    true
}

const fn default_connect_timeout() -> DurationStr {
    DurationStr::from_secs(2)
}

const fn default_pool_max_idle() -> u32 {
    64
}
const fn default_pool_idle_timeout() -> DurationStr {
    DurationStr::from_secs(60)
}
const fn default_pool_max_lifetime() -> DurationStr {
    DurationStr::from_secs(600)
}

fn default_hc_path() -> String {
    "/healthz".into()
}
const fn default_hc_timeout() -> DurationStr {
    DurationStr::from_secs(2)
}
const fn default_hc_unhealthy_threshold() -> u32 {
    3
}
const fn default_hc_healthy_threshold() -> u32 {
    1
}

const fn default_timeout_connect() -> DurationStr {
    DurationStr::from_secs(2)
}
const fn default_timeout_read() -> DurationStr {
    DurationStr::from_secs(30)
}
const fn default_timeout_write() -> DurationStr {
    DurationStr::from_secs(30)
}
const fn default_timeout_total() -> DurationStr {
    DurationStr::from_secs(60)
}
