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
    check_logic(cfg)?;
    check_filesystem(cfg)?;
    Ok(())
}

/// Pure, in-memory validation. No I/O.
fn check_logic(cfg: &Config) -> Result<(), ConfigError> {
    if cfg.routes.is_empty() {
        return Err(ConfigError::NoRoutes);
    }

    // Upstream addresses present, names unique.
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
        let cfg: Config = toml::from_str(toml_text)?;
        check_logic(&cfg)?;
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
}
