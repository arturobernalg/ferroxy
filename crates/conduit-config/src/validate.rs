//! Cross-field semantic validation.
//!
//! Split into two phases so unit tests can exercise the pure-logic phase
//! without touching the filesystem:
//!
//! - [`check_logic`] inspects only in-memory fields. Pure, total, fast.
//! - [`check_filesystem`] verifies that paths referenced by config exist
//!   and are regular files. Side-effecting; gated behind `check_logic` so
//!   we never report a missing file when a logic error is the real cause.
//!
//! TLS *parsing* (does the cert actually load as PEM?) is **not** done here.
//! That logic belongs in `conduit-transport`, the layer that owns the rustls
//! types. Phase 0 stops at "the file exists." See engineering charter
//! deviation note in the Phase 0 report.

use std::collections::HashSet;

use crate::error::ConfigError;
use crate::schema::Config;

/// Run every semantic check. Logic checks first, then filesystem.
pub(crate) fn check(cfg: &Config) -> Result<(), ConfigError> {
    let opts = ValidationOptions {
        allow_insecure_tls: std::env::var_os("CONDUIT_ALLOW_INSECURE_TLS")
            .is_some_and(|v| v == "1"),
    };
    check_logic(cfg, opts)?;
    check_filesystem(cfg)?;
    Ok(())
}

/// Knobs for the logic-only validation pass that can't come from
/// the config file itself (because the choice has security
/// implications and the operator must have made it deliberately).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ValidationOptions {
    /// Permit `[upstream.tls] verify = false`. Set from
    /// `CONDUIT_ALLOW_INSECURE_TLS=1` at the public entry point;
    /// tests construct this directly.
    pub(crate) allow_insecure_tls: bool,
}

/// Pure, in-memory validation. No I/O.
fn check_logic(cfg: &Config, opts: ValidationOptions) -> Result<(), ConfigError> {
    if cfg.routes.is_empty() {
        return Err(ConfigError::NoRoutes);
    }

    // Upstream addresses present, names unique, mTLS pair sane,
    // verify=false explicitly opted into, health-check durations > 0.
    let mut seen_names: HashSet<&str> = HashSet::with_capacity(cfg.upstreams.len());
    for (i, u) in cfg.upstreams.iter().enumerate() {
        if u.addrs.is_empty() {
            return Err(ConfigError::UpstreamNoAddrs {
                index: i,
                name: u.name.clone(),
            });
        }
        if !seen_names.insert(u.name.as_str()) {
            return Err(ConfigError::DuplicateUpstream {
                name: u.name.clone(),
            });
        }
        if let Some(tls) = &u.tls {
            // mTLS pair: both or neither.
            if tls.client_cert.is_some() != tls.client_key.is_some() {
                return Err(ConfigError::UpstreamMtlsMismatch {
                    name: u.name.clone(),
                });
            }
            // verify=false is opt-in via env. Memory note from
            // 2026-05-01 user feedback: never let an "InsecureVerifier"
            // sneak into a prod config silently.
            if !tls.verify && !opts.allow_insecure_tls {
                return Err(ConfigError::UpstreamInsecureTlsNotOptedIn {
                    name: u.name.clone(),
                });
            }
        }
        if let Some(hc) = &u.health_check {
            let zero = std::time::Duration::ZERO;
            if std::time::Duration::from(hc.interval) == zero {
                return Err(ConfigError::HealthCheckBadDuration {
                    name: u.name.clone(),
                    field: "interval",
                });
            }
            if std::time::Duration::from(hc.timeout) == zero {
                return Err(ConfigError::HealthCheckBadDuration {
                    name: u.name.clone(),
                    field: "timeout",
                });
            }
        }
    }

    // Routes resolve to existing upstreams; match block is non-empty.
    for (i, r) in cfg.routes.iter().enumerate() {
        if !seen_names.contains(r.upstream.as_str()) {
            return Err(ConfigError::UnknownUpstream {
                index: i,
                name: r.upstream.clone(),
            });
        }

        let m = &r.match_;
        let has_host = m.host.is_some();
        let has_path = m.path_prefix.is_some() || m.path_exact.is_some() || m.path_regex.is_some();
        if !has_host && !has_path {
            return Err(ConfigError::RouteMatchEmpty { index: i });
        }
        if m.path_prefix.is_some() && m.path_exact.is_some() {
            return Err(ConfigError::RoutePathConflict { index: i });
        }

        if let Some(retry) = &r.retry {
            if retry.attempts == 0 {
                return Err(ConfigError::RetryAttemptsZero { index: i });
            }
        }
    }

    // TLS section is required if we plan to listen on https or h3.
    let needs_tls = !cfg.server.listen_https.is_empty() || !cfg.server.listen_h3.is_empty();
    match (&cfg.tls, needs_tls) {
        (None, true) => return Err(ConfigError::TlsRequired),
        (Some(t), _) => {
            if t.certs.is_empty() {
                return Err(ConfigError::TlsNoCerts);
            }
        }
        (None, false) => {}
    }

    Ok(())
}

/// Filesystem-touching validation. Runs after [`check_logic`] succeeds.
fn check_filesystem(cfg: &Config) -> Result<(), ConfigError> {
    let Some(tls) = &cfg.tls else {
        return Ok(());
    };
    for c in &tls.certs {
        if !c.cert.is_file() {
            return Err(ConfigError::CertMissing {
                sni: c.sni.clone(),
                path: c.cert.clone(),
            });
        }
        if !c.key.is_file() {
            return Err(ConfigError::KeyMissing {
                sni: c.sni.clone(),
                path: c.key.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Logic-only tests. Filesystem checks are exercised by the
    //! `tests/` integration tests where fixture files exist.
    use super::*;

    fn parse_logic(toml_text: &str) -> Result<Config, ConfigError> {
        parse_logic_with(toml_text, ValidationOptions::default())
    }

    fn parse_logic_with(toml_text: &str, opts: ValidationOptions) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(toml_text)?;
        check_logic(&cfg, opts)?;
        Ok(cfg)
    }

    const MIN_VALID: &str = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com", path_prefix = "/" }
upstream = "api"
"#;

    #[test]
    fn minimal_is_valid() {
        let cfg = parse_logic(MIN_VALID).unwrap();
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(cfg.routes.len(), 1);
    }

    #[test]
    fn empty_routes_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"
"#;
        assert!(matches!(parse_logic(toml), Err(ConfigError::NoRoutes)));
    }

    #[test]
    fn unknown_upstream_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "missing"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::UnknownUpstream { .. })
        ));
    }

    #[test]
    fn duplicate_upstream_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[upstream]]
name = "api"
addrs = ["10.0.0.2:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::DuplicateUpstream { .. })
        ));
    }

    #[test]
    fn upstream_no_addrs_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = []
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::UpstreamNoAddrs { .. })
        ));
    }

    #[test]
    fn empty_match_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = {}
upstream = "api"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::RouteMatchEmpty { .. })
        ));
    }

    #[test]
    fn path_conflict_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { path_prefix = "/", path_exact = "/foo" }
upstream = "api"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::RoutePathConflict { .. })
        ));
    }

    #[test]
    fn retry_attempts_zero_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
retry = { attempts = 0 }
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::RetryAttemptsZero { .. })
        ));
    }

    #[test]
    fn https_without_tls_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_https = ["0.0.0.0:443"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(parse_logic(toml), Err(ConfigError::TlsRequired)));
    }

    #[test]
    fn h3_without_tls_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_h3 = ["0.0.0.0:443"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(parse_logic(toml), Err(ConfigError::TlsRequired)));
    }

    #[test]
    fn empty_tls_certs_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[tls]
certs = []

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(parse_logic(toml), Err(ConfigError::TlsNoCerts)));
    }

    #[test]
    fn unknown_field_rejected_at_toml_layer() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
mystery_key = true
"#;
        let r: Result<Config, _> = toml::from_str(toml);
        assert!(r.is_err());
    }

    #[test]
    fn unknown_field_in_upstream_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"
made_up_field = 1
"#;
        let r: Result<Config, _> = toml::from_str(toml);
        assert!(r.is_err());
    }

    #[test]
    fn upstream_mtls_pair_required() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"
[upstream.tls]
client_cert = "/etc/conduit/c.pem"
verify = true

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::UpstreamMtlsMismatch { .. })
        ));
    }

    #[test]
    fn upstream_insecure_tls_requires_env_opt_in() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"
[upstream.tls]
verify = false

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        // Default options: insecure TLS rejected.
        assert!(matches!(
            parse_logic(toml),
            Err(ConfigError::UpstreamInsecureTlsNotOptedIn { .. })
        ));
        // Opt-in flag set: same config validates.
        let result = parse_logic_with(
            toml,
            ValidationOptions {
                allow_insecure_tls: true,
            },
        );
        assert!(
            result.is_ok(),
            "with opt-in, should validate; got {result:?}"
        );
    }

    #[test]
    fn health_check_zero_interval_rejected() {
        let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"
health_check = { path = "/health", interval = "0s", timeout = "1s" }

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
        match parse_logic(toml) {
            Err(ConfigError::HealthCheckBadDuration { field, .. }) => {
                assert_eq!(field, "interval");
            }
            other => panic!("expected HealthCheckBadDuration, got {other:?}"),
        }
    }
}
