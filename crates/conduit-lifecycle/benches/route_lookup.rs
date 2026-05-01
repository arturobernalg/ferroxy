//! Route-table lookup microbench.
//!
//! Charter rule: hot-path changes ship a no-regression criterion
//! bench. The route table is on the hot path (one lookup per
//! request), and the P5.x trie-equivalent redesign needs a number to
//! defend against future regressions.
//!
//! Bench shape: build a 100-host × 10-prefix table, then time:
//!   - exact-host + exact-path hit
//!   - exact-host + longest-prefix hit
//!   - wildcard-fallback (host not registered)
//!
//! Run locally:
//!
//! ```bash
//! cargo bench -p conduit-lifecycle --bench route_lookup
//! ```

use std::fmt::Write;
use std::hint::black_box;

use conduit_lifecycle::RouteTable;
use criterion::{criterion_group, criterion_main, Criterion};

/// Build a synthetic config with `hosts` × `prefixes_per_host` routes
/// plus a wildcard fallback. Each host has prefixes `/p0/`, `/p1/`, …
/// in declaration order; each route forwards to the same dummy
/// upstream so we don't need a real backend.
fn build_table(hosts: usize, prefixes_per_host: usize) -> RouteTable {
    let mut toml = String::with_capacity(4096);
    toml.push_str(
        "[server]\n\
         admin_listen = \"127.0.0.1:9090\"\n\
         metrics_listen = \"127.0.0.1:9091\"\n\
         listen_http = [\"0.0.0.0:80\"]\n\n\
         [[upstream]]\n\
         name = \"u\"\n\
         addrs = [\"10.0.0.1:8080\"]\n\
         protocol = \"http1\"\n\n",
    );
    for h in 0..hosts {
        for p in 0..prefixes_per_host {
            let _ = writeln!(
                toml,
                "[[route]]\nmatch = {{ host = \"h{h}.example.com\", path_prefix = \"/p{p}/\" }}\nupstream = \"u\"\n",
            );
        }
    }
    // Wildcard fallback (no host, broadest prefix).
    toml.push_str("[[route]]\nmatch = { path_prefix = \"/\" }\nupstream = \"u\"\n");
    let cfg = conduit_config::parse(&toml).expect("parse synthetic config");
    RouteTable::from_config(&cfg)
}

fn bench_route_lookup(c: &mut Criterion) {
    let t = build_table(100, 10);
    let headers = http::HeaderMap::new();

    c.bench_function("route_lookup/exact_host_first_prefix", |b| {
        b.iter(|| {
            black_box(t.find(
                black_box(Some("h0.example.com")),
                black_box("/p0/x"),
                &headers,
            ));
        });
    });

    c.bench_function("route_lookup/exact_host_last_prefix", |b| {
        b.iter(|| {
            black_box(t.find(
                black_box(Some("h99.example.com")),
                black_box("/p9/x"),
                &headers,
            ));
        });
    });

    c.bench_function("route_lookup/wildcard_fallback", |b| {
        b.iter(|| {
            // Host has no exact bucket → wildcard fallback.
            black_box(t.find(
                black_box(Some("unknown.example.com")),
                black_box("/anything"),
                &headers,
            ));
        });
    });
}

criterion_group!(benches, bench_route_lookup);
criterion_main!(benches);
