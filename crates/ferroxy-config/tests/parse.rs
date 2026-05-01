//! Integration tests covering the public API: `parse` and `load`.
//!
//! Logic-only validation cases live in `validate::tests`; this file covers
//! the file-system phase and the public entry points.

use std::path::PathBuf;

use ferroxy_config::{parse, ConfigError};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn https_with_present_cert_files_validates() {
    let cert = fixture("dummy.crt");
    let key = fixture("dummy.key");
    let toml = format!(
        r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_https = ["0.0.0.0:443"]

[tls]
certs = [{{ sni = "example.com", cert = "{}", key = "{}" }}]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = {{ host = "example.com", path_prefix = "/" }}
upstream = "api"
"#,
        cert.display(),
        key.display()
    );
    let cfg = parse(&toml).expect("config should validate end-to-end");
    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.routes.len(), 1);
}

#[test]
fn missing_cert_file_rejected() {
    let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_https = ["0.0.0.0:443"]

[tls]
certs = [{ sni = "example.com", cert = "/does/not/exist.crt", key = "/does/not/exist.key" }]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = { host = "example.com" }
upstream = "api"
"#;
    let err = parse(toml).unwrap_err();
    assert!(
        matches!(err, ConfigError::CertMissing { .. }),
        "got {err:?}"
    );
}

#[test]
fn missing_key_file_rejected() {
    let cert = fixture("dummy.crt");
    let toml = format!(
        r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_https = ["0.0.0.0:443"]

[tls]
certs = [{{ sni = "example.com", cert = "{}", key = "/does/not/exist.key" }}]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "http1"

[[route]]
match = {{ host = "example.com" }}
upstream = "api"
"#,
        cert.display()
    );
    let err = parse(&toml).unwrap_err();
    assert!(matches!(err, ConfigError::KeyMissing { .. }), "got {err:?}");
}

#[test]
fn timeouts_and_retry_round_trip() {
    let toml = r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080"]
protocol = "h2"
connect_timeout = "500ms"

[[route]]
match = { host = "api.example.com" }
upstream = "api"
timeouts = { connect = "1s", read = "5s", write = "5s", total = "10s" }
retry = { attempts = 3, on_status = [502, 503, 504], on_connect_error = true }
"#;
    let cfg = parse(toml).expect("parse");
    let r = &cfg.routes[0];
    assert_eq!(r.timeouts.read.get(), std::time::Duration::from_secs(5));
    let retry = r.retry.as_ref().expect("retry block present");
    assert_eq!(retry.attempts, 3);
    assert_eq!(retry.on_status, vec![502, 503, 504]);
    assert!(retry.on_connect_error);
}

#[test]
fn load_reads_from_disk() {
    let cert = fixture("dummy.crt");
    let key = fixture("dummy.key");
    let toml_text = format!(
        r#"
[server]
admin_listen = "127.0.0.1:9090"
metrics_listen = "127.0.0.1:9091"
listen_http = ["0.0.0.0:80"]
listen_https = ["0.0.0.0:443"]

[tls]
certs = [{{ sni = "example.com", cert = "{}", key = "{}" }}]

[[upstream]]
name = "api"
addrs = ["10.0.0.1:8080", "10.0.0.2:8080"]
protocol = "http1"
lb = "p2c"

[[route]]
match = {{ host = "example.com", path_prefix = "/" }}
upstream = "api"
"#,
        cert.display(),
        key.display()
    );

    // Write to a temp file under the test crate's target dir to avoid /tmp races.
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    tmp.push(format!("ferroxy-config-test-{pid}.toml"));
    std::fs::write(&tmp, &toml_text).unwrap();
    let result = ferroxy_config::load(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let cfg = result.expect("load");
    assert_eq!(cfg.upstreams[0].addrs.len(), 2);
}
