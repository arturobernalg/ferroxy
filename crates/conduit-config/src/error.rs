//! Typed errors returned by config load/parse/validate.

use std::path::PathBuf;

/// Everything that can go wrong while loading or validating a configuration.
///
/// Errors come in two layers:
///
/// 1. **Structural** — `Read`, `Toml`. The bytes either could not be obtained
///    or could not be parsed as valid TOML matching the schema. These are
///    produced by the standard library and the `toml` crate respectively.
/// 2. **Semantic** — every other variant. The TOML parsed cleanly but
///    violates a cross-field invariant.
///
/// Each semantic variant names exactly one configuration object (route N,
/// upstream M, cert S) and one fault. No variant aggregates.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("failed to read config file `{path}`")]
    Read {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The TOML did not parse, or did not match the schema.
    ///
    /// `toml::de::Error` already includes line/column information.
    #[error("config syntax error")]
    Toml(#[from] toml::de::Error),

    /// A `[[route]]` references an upstream name that is not declared.
    #[error("route #{index} references unknown upstream `{name}`")]
    UnknownUpstream {
        /// Zero-based index of the route in the routes table.
        index: usize,
        /// The unresolved upstream name.
        name: String,
    },

    /// An `[[upstream]]` was declared with no addresses.
    #[error("upstream #{index} `{name}` has no addresses")]
    UpstreamNoAddrs {
        /// Zero-based index of the upstream in the upstreams table.
        index: usize,
        /// The upstream name (still useful for grep-style debugging).
        name: String,
    },

    /// Two upstreams share a name. Names must be unique because routes
    /// reference upstreams by name.
    #[error("duplicate upstream name `{name}`")]
    DuplicateUpstream {
        /// The duplicated name.
        name: String,
    },

    /// The configuration declared no `[[route]]` entries; a proxy with no
    /// routes cannot serve traffic and is rejected at load time.
    #[error("config has no [[route]] entries")]
    NoRoutes,

    /// The server section asked for HTTPS or H3 listeners but no `[tls]`
    /// section was provided.
    #[error("server.listen_https or server.listen_h3 is set but [tls] is missing")]
    TlsRequired,

    /// `[tls]` was provided with no certificates.
    #[error("[tls] section has no certs")]
    TlsNoCerts,

    /// A cert file referenced by a TLS entry is not present.
    #[error("cert file `{path}` for sni `{sni}` does not exist or is not a regular file")]
    CertMissing {
        /// SNI hostname this cert serves.
        sni: String,
        /// The path that was checked.
        path: PathBuf,
    },

    /// A key file referenced by a TLS entry is not present.
    #[error("key file `{path}` for sni `{sni}` does not exist or is not a regular file")]
    KeyMissing {
        /// SNI hostname this key serves.
        sni: String,
        /// The path that was checked.
        path: PathBuf,
    },

    /// A `[[route]]`'s `match` block contained none of `host`, `path_prefix`,
    /// `path_exact`, `path_regex`. An empty match would catch every request,
    /// which is almost certainly a configuration mistake.
    #[error("route #{index} match block has no host or path constraint")]
    RouteMatchEmpty {
        /// Zero-based index of the route in the routes table.
        index: usize,
    },

    /// A `[[route]]` set both `path_prefix` and `path_exact`; only one is
    /// meaningful.
    #[error("route #{index} sets both path_prefix and path_exact")]
    RoutePathConflict {
        /// Zero-based index of the route in the routes table.
        index: usize,
    },

    /// A retry policy specified `attempts = 0`; that disables retry, which
    /// should be expressed by omitting the retry block entirely.
    #[error("route #{index} has retry.attempts = 0; omit the retry block instead")]
    RetryAttemptsZero {
        /// Zero-based index of the route in the routes table.
        index: usize,
    },

    /// `[upstream.tls]` set `client_cert` without `client_key` (or
    /// vice-versa). mTLS requires both.
    #[error(
        "upstream `{name}` tls.client_cert and tls.client_key must both be set, or both unset"
    )]
    UpstreamMtlsMismatch {
        /// Name of the offending upstream.
        name: String,
    },

    /// `[upstream.tls]` set `verify = false` without the
    /// `CONDUIT_ALLOW_INSECURE_TLS=1` environment variable. The
    /// "trust everything" verifier must be opted into explicitly so
    /// it can't sneak into a production deployment.
    #[error(
        "upstream `{name}` tls.verify = false rejected; set \
         CONDUIT_ALLOW_INSECURE_TLS=1 in the environment to allow"
    )]
    UpstreamInsecureTlsNotOptedIn {
        /// Name of the offending upstream.
        name: String,
    },

    /// `[upstream.health_check]` had a non-positive interval or timeout.
    /// A zero interval would spin the prober as fast as the runtime
    /// allows; a zero timeout would mark every probe as failed.
    #[error("upstream `{name}` health_check.{field} must be > 0")]
    HealthCheckBadDuration {
        /// Name of the offending upstream.
        name: String,
        /// Field whose value was non-positive (`interval` or `timeout`).
        field: &'static str,
    },
}
