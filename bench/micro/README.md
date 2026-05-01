# Microbenchmarks

Microbenchmarks measure one thing on the per-request path in isolation.
A number here is publishable on its own merits — the methodology is
bounded and easy to defend. **System-level comparison numbers (RPS,
p99 vs nginx)** belong in [`bench/compare/`](../compare/) and require
the full methodology bar in [`BENCHMARKS.md`](../../BENCHMARKS.md).

## Index

| Bench | Crate | Source | Run command |
|-------|-------|--------|-------------|
| `route_lookup` | `conduit-lifecycle` | [`crates/conduit-lifecycle/benches/route_lookup.rs`](../../crates/conduit-lifecycle/benches/route_lookup.rs) | `cargo bench -p conduit-lifecycle --bench route_lookup` |

More benches land here as hot-path components grow them. Each one
records its baseline number under "Baseline" below; PRs that change
the implementation must rerun and update.

## Baseline numbers

The following numbers are from the host the README on `main` was
last rebuilt on. Numbers are sensitive to CPU model and frequency
scaling — treat them as the bar to beat on the same hardware,
not as portable absolutes.

### `route_lookup`

Synthetic table: 100 hosts × 10 path-prefix routes per host + a
wildcard fallback. 1001 routes total. Compiled in `--release`
profile (`opt-level = 3`, `lto = "thin"`). Criterion full run
(median across 100 iterations).

| Scenario                          | Median time |
|-----------------------------------|-------------|
| Exact host, first prefix in bucket | 25 ns       |
| Exact host, last prefix in bucket  | 44 ns       |
| Wildcard fallback (host miss)      | 25 ns       |

Interpretation: the routing layer adds at most ~44 ns to the request
path at this table size. The longest-prefix scan inside the bucket
is what stretches the upper bound — the bucket holds 10 prefix
entries and our walk is linear within a bucket. A trie would flatten
that to O(|path|) but the constant factor at this scale is not yet
worth the trie machinery.

#### Baseline history

| Date       | first | last | wildcard | Reason                                                          |
|------------|------:|-----:|---------:|-----------------------------------------------------------------|
| 2026-05-01 |  37 ns | 50 ns |  32 ns | Initial baseline (linear scan replaced by host bucket).         |
| 2026-05-01 |  40 ns | 52 ns |  33 ns | Header-routing landed; per-slot Vec + headers_match call added. |
| 2026-05-01 |  25 ns | 44 ns |  25 ns | `lookup_bucket` skips `to_ascii_lowercase` alloc for lowercase hosts (the common case). |
| 2026-05-01 |  25 ns | 42 ns |  24 ns | `release-bench` profile (fat-LTO + codegen-units=1). Minor on the routing slice; cross-crate inlining win shows up in end-to-end RPS, not here. |

Future PRs that touch the routing hot path must rerun this bench
and update the table; a regression beyond noise needs a commit
message that justifies the trade.

### Methodology

- `cargo bench -p conduit-lifecycle --bench route_lookup -- --quick`
- Criterion 0.5 with HTML reports
- Release profile (workspace-wide settings: `opt-level = 3`,
  `lto = "thin"`, `codegen-units = 1`, `debug = "line-tables-only"`)
- `black_box` on every input to defeat hoisting
- Results are per-iteration medians from criterion's analysis output

To reproduce:
```bash
cargo bench -p conduit-lifecycle --bench route_lookup -- --quick
```

For a full run (longer; produces stable confidence intervals):
```bash
cargo bench -p conduit-lifecycle --bench route_lookup
```

## When to add a microbench

Charter rule: hot-path changes ship a no-regression criterion bench.
"Hot path" means anywhere in the per-request execution: route
lookup, dispatch, breaker check, body conversion, header parse,
upstream URI rewrite. Cold paths (config load, admin endpoints,
shutdown) do not gate on bench numbers.

When a bench is added:

1. Place it in `crates/<owner>/benches/<name>.rs`.
2. Declare it in the crate's `Cargo.toml` (`[[bench]] name = "<name>" harness = false`).
3. Add an entry to the index above with its run command.
4. Run it once on the dev box and record the baseline numbers
   in this file's "Baseline" section.
5. Update the baseline whenever the implementation changes the hot
   path — the commit message must justify the delta (or explain
   the win).
