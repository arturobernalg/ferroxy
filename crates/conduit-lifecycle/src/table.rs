//! Route table + upstream map + dispatch.
//!
//! All three types share the same lifetime: they are constructed
//! once at config load and consumed (read-only) on the hot path.
//! The hot path performs:
//!   1. `RouteTable::find(&host, &path)`  — O(1) host bucket lookup,
//!      then O(1) exact-path lookup or O(prefixes-per-bucket) longest
//!      prefix scan. Buckets sort prefixes longest-first at construction
//!      time so the scan returns the most-specific match.
//!   2. `UpstreamMap::get(name)`          — O(1) hash lookup
//!   3. `Upstream::forward(req)`          — async, real network I/O
//!
//! No `Arc<Mutex<…>>` or any other synchronisation primitive lives
//! on this path. The map and table are immutable after construction;
//! the binary holds them in an `Arc` so all worker tasks share one
//! copy without locking. Hot-reload (P10) replaces the `Arc<…>`
//! atomically via `arc-swap`.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::header::HOST;
use hyper::body::Incoming;

use conduit_proto::{Body, Request, Response};
use conduit_upstream::{ForwardError, Upstream};

/// One route. Constructed from the config struct via [`Route::from_config`].
#[derive(Debug, Clone)]
pub struct Route {
    /// Optional virtual-host match. `None` matches any host.
    pub host: Option<String>,
    /// Path matcher.
    pub path: PathMatch,
    /// Header predicates. **Every** entry must match for the route to
    /// be a candidate. Pre-lowercased header name + exact value
    /// match. Empty list means no header predicate.
    pub header_matches: Vec<(http::HeaderName, http::HeaderValue)>,
    /// Name of the upstream to dispatch to. Resolved at config-load
    /// time against the [`UpstreamMap`]; if absent, the validator in
    /// `conduit-config` already rejected the document.
    pub upstream: String,
    /// Wall-clock cap on the entire request, derived from
    /// `[[route]] timeouts.total`. The dispatcher wraps the upstream
    /// forward in a `tokio::time::timeout` of this duration.
    pub total_timeout: std::time::Duration,
}

impl Route {
    /// Test whether `headers` satisfies every entry in this route's
    /// `header_matches`. A route with no header predicates always
    /// matches; otherwise every name+value pair must be present.
    fn headers_match(&self, headers: &http::HeaderMap) -> bool {
        for (name, want) in &self.header_matches {
            match headers.get(name) {
                Some(got) if got == want => {}
                _ => return false,
            }
        }
        true
    }
}

/// Path-matching kinds.
#[derive(Debug, Clone)]
pub enum PathMatch {
    /// Match any path.
    Any,
    /// `path_prefix = "/api"` — request path starts with this prefix.
    Prefix(String),
    /// `path_exact = "/healthz"` — request path equals this string.
    Exact(String),
    /// `path_regex = "^/v(?P<v>\d+)/.+"` — request path is matched
    /// against the compiled regex. Compiled once at config-load.
    Regex(regex::Regex),
}

impl Route {
    /// Build a route from the config struct. Picks the first non-empty
    /// path matcher in priority order: exact > prefix > regex > any.
    /// An invalid regex falls back to `PathMatch::Any` and logs an
    /// error — the validator in conduit-config rejects unparseable
    /// regexes at load time, so this is purely defensive.
    pub fn from_config(cfg: &conduit_config::Route) -> Self {
        let path = if let Some(s) = cfg.match_.path_exact.as_ref() {
            PathMatch::Exact(s.clone())
        } else if let Some(s) = cfg.match_.path_prefix.as_ref() {
            PathMatch::Prefix(s.clone())
        } else if let Some(s) = cfg.match_.path_regex.as_ref() {
            match regex::Regex::new(s) {
                Ok(re) => PathMatch::Regex(re),
                Err(e) => {
                    tracing::error!(
                        regex = %s,
                        error = %e,
                        "route path_regex did not compile; route will match any path",
                    );
                    PathMatch::Any
                }
            }
        } else {
            PathMatch::Any
        };
        // Translate header predicates from the schema into hot-path
        // friendly types. `try_from` rejects empty / non-ascii names
        // and values; the validator already screened them.
        let header_matches = cfg
            .match_
            .headers
            .iter()
            .filter_map(|hm| {
                let name = http::HeaderName::try_from(hm.name.as_str()).ok()?;
                let value = http::HeaderValue::try_from(hm.value.as_str()).ok()?;
                Some((name, value))
            })
            .collect();
        Self {
            host: cfg.match_.host.clone(),
            path,
            header_matches,
            upstream: cfg.upstream.clone(),
            total_timeout: cfg.timeouts.total.into(),
        }
    }
}

/// Per-host bucket of routes, indexed for O(1) exact-path lookup,
/// length-sorted prefix scanning, and ordered regex evaluation. Each
/// slot holds a `Vec<Route>` in declaration order so header
/// predicates can fall through to less-specific routes.
#[derive(Debug, Clone, Default)]
struct HostBucket {
    /// Exact-path routes, keyed by the literal path string. Multiple
    /// routes can share the same path if they differ in header
    /// predicates; the vec preserves declaration order.
    by_exact: HashMap<String, Vec<Route>>,
    /// Prefix routes sorted by prefix length descending (most-specific
    /// first), with declaration order preserved as the tie-breaker.
    /// Multiple routes per prefix are allowed for the same reason as
    /// `by_exact`.
    prefix_sorted: Vec<(String, Vec<Route>)>,
    /// Regex routes in declaration order. Tried after exact + prefix
    /// fail; the first regex whose header predicates also match wins.
    regex_routes: Vec<Route>,
    /// `PathMatch::Any` routes in declaration order. The first whose
    /// header predicates also match wins.
    any: Vec<Route>,
}

impl HostBucket {
    /// Insert a route into the bucket. Per the charter's "first in
    /// declaration order wins" rule, the order within each slot's
    /// `Vec<Route>` is the order routes were declared.
    fn insert(&mut self, r: Route) {
        match &r.path {
            PathMatch::Exact(p) => {
                self.by_exact.entry(p.clone()).or_default().push(r);
            }
            PathMatch::Prefix(p) => {
                if let Some(slot) = self.prefix_sorted.iter_mut().find(|(pp, _)| pp == p) {
                    slot.1.push(r);
                } else {
                    self.prefix_sorted.push((p.clone(), vec![r]));
                }
            }
            PathMatch::Regex(_) => {
                self.regex_routes.push(r);
            }
            PathMatch::Any => {
                self.any.push(r);
            }
        }
    }

    /// Sort `prefix_sorted` longest-first. Stable so declaration order
    /// breaks ties between prefixes of equal length.
    fn finalise(&mut self) {
        self.prefix_sorted
            .sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    }

    fn find(&self, path: &str, headers: &http::HeaderMap) -> Option<&Route> {
        if let Some(slot) = self.by_exact.get(path) {
            if let Some(r) = slot.iter().find(|r| r.headers_match(headers)) {
                return Some(r);
            }
        }
        for (prefix, slot) in &self.prefix_sorted {
            if path.starts_with(prefix.as_str()) {
                if let Some(r) = slot.iter().find(|r| r.headers_match(headers)) {
                    return Some(r);
                }
            }
        }
        for r in &self.regex_routes {
            if let PathMatch::Regex(re) = &r.path {
                if re.is_match(path) && r.headers_match(headers) {
                    return Some(r);
                }
            }
        }
        self.any.iter().find(|r| r.headers_match(headers))
    }
}

/// Routes grouped by host for O(1) host lookup, then O(1) exact-path
/// or O(prefixes-per-host) longest-prefix selection. The wildcard
/// bucket (`host = None`) is consulted as a fallback when the
/// host-keyed bucket does not match.
///
/// Lookup semantics match the linear-scan implementation that
/// preceded this: declaration order is the tie-breaker within a
/// bucket, and host-keyed routes are preferred over wildcard routes
/// for the same path.
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    /// Routes whose `host` was a concrete string. Keys are stored
    /// pre-lowercased so lookups can fold case in a single pass.
    by_host: HashMap<String, HostBucket>,
    /// Routes whose `host` was `None` — match any request host.
    wildcard: HostBucket,
    /// Total route count; cheap accessor for observability.
    len: usize,
}

impl RouteTable {
    /// Build from the validated config document. Routes are grouped
    /// by host into buckets and indexed for O(1) host lookup.
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        let mut t = Self::default();
        for r in cfg.routes.iter().map(Route::from_config) {
            t.len += 1;
            match r.host.as_deref() {
                Some(h) => {
                    let key = h.to_ascii_lowercase();
                    t.by_host.entry(key).or_default().insert(r);
                }
                None => t.wildcard.insert(r),
            }
        }
        for bucket in t.by_host.values_mut() {
            bucket.finalise();
        }
        t.wildcard.finalise();
        t
    }

    /// Find the route that matches `(host, path, headers)`. Tries
    /// the host-keyed bucket first (case-folded), then falls back to
    /// the wildcard bucket. Within a bucket, exact > longest-prefix >
    /// regex > any; routes with header predicates are skipped if any
    /// predicate fails, falling through to less-specific routes.
    /// Returns `None` if no route matches.
    pub fn find(
        &self,
        host: Option<&str>,
        path: &str,
        headers: &http::HeaderMap,
    ) -> Option<&Route> {
        if let Some(h) = host {
            if let Some(bucket) = self.lookup_bucket(h) {
                if let Some(r) = bucket.find(path, headers) {
                    return Some(r);
                }
            }
        }
        self.wildcard.find(path, headers)
    }

    /// Look up a host bucket. Avoids allocating a lowercased copy of
    /// the host when the input is already all-ASCII-lowercase (the
    /// common case for properly-configured clients). Mixed-case hosts
    /// fall back to a one-shot allocation; this is rare in practice.
    fn lookup_bucket(&self, host: &str) -> Option<&HostBucket> {
        if host.bytes().all(|b| !b.is_ascii_uppercase()) {
            return self.by_host.get(host);
        }
        let lower = host.to_ascii_lowercase();
        self.by_host.get(&lower)
    }

    /// Number of routes (for observability / tests).
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the table has no routes. Reject at config-validate
    /// time, but cheap to expose.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Upstreams indexed by name.
#[derive(Clone, Default)]
pub struct UpstreamMap {
    by_name: HashMap<String, Upstream>,
}

impl std::fmt::Debug for UpstreamMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamMap")
            .field("len", &self.by_name.len())
            .finish_non_exhaustive()
    }
}

impl UpstreamMap {
    /// Build an empty map. Use [`UpstreamMap::insert`] to register
    /// upstreams as they are constructed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an upstream under `name`. Returns the previous binding
    /// if one existed.
    pub fn insert(&mut self, name: impl Into<String>, upstream: Upstream) -> Option<Upstream> {
        self.by_name.insert(name.into(), upstream)
    }

    /// Iterate `(name, upstream)` pairs in arbitrary order. Used by
    /// the admin server to render the live pool stats endpoint.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Upstream)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Look up an upstream by name. Cheap (`HashMap` get).
    pub fn get(&self, name: &str) -> Option<&Upstream> {
        self.by_name.get(name)
    }

    /// Build directly from a config document by constructing default
    /// [`Upstream`]s for every `[[upstream]]` entry. Retry policies
    /// from per-route blocks are *not* applied here — they get applied
    /// per request in [`Dispatch::handle`] (since the same upstream
    /// can be referenced by routes with different retry rules).
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        let mut map = Self::new();
        for u in &cfg.upstreams {
            // Per-upstream connect timeout from `[upstream] connect_timeout`.
            // Configures hyper-util's HttpConnector; the per-route
            // total timeout is a separate, larger budget owned by the
            // dispatcher.
            let connect: std::time::Duration = u.connect_timeout.into();
            // Translate the schema's `[upstream.tls]` block into the
            // hot-path types. `None` → default verifier + webpki
            // roots. Failures loading per-upstream PEMs log + skip
            // the upstream rather than half-load it.
            let tls_opts = u
                .tls
                .as_ref()
                .map(|t| conduit_upstream::UpstreamTlsOptions {
                    ca: t.ca.clone(),
                    client_cert: t.client_cert.clone(),
                    client_key: t.client_key.clone(),
                    verify: t.verify,
                });
            let upstream = match Upstream::with_tls(
                conduit_upstream::RetryPolicy::default(),
                u.addrs.clone(),
                conduit_upstream::BreakerConfig::default(),
                Some(connect),
                tls_opts,
            ) {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!(
                        upstream = %u.name,
                        error = %e,
                        "failed to build upstream TLS config; skipping",
                    );
                    continue;
                }
            };
            map.insert(u.name.clone(), upstream);
        }
        map
    }
}

/// Per-worker dispatcher: an [`Arc`]-shared bundle of route table +
/// upstream map. Cloning is a refcount bump.
#[derive(Clone)]
pub struct Dispatch {
    inner: Arc<DispatchInner>,
}

struct DispatchInner {
    routes: RouteTable,
    upstreams: UpstreamMap,
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch")
            .field("routes", &self.inner.routes)
            .field("upstreams", &self.inner.upstreams)
            .finish()
    }
}

impl Dispatch {
    /// Build from a route table and an upstream map.
    pub fn new(routes: RouteTable, upstreams: UpstreamMap) -> Self {
        Self {
            inner: Arc::new(DispatchInner { routes, upstreams }),
        }
    }

    /// Read-only access to the upstream map. Used by the admin server
    /// to render `/upstreams`.
    pub fn upstreams(&self) -> &UpstreamMap {
        &self.inner.upstreams
    }

    /// Read-only access to the route table.
    pub fn routes(&self) -> &RouteTable {
        &self.inner.routes
    }

    /// Build from a validated config document end-to-end.
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        Self::new(RouteTable::from_config(cfg), UpstreamMap::from_config(cfg))
    }

    /// Look up the route for the supplied request and dispatch it to
    /// the named upstream's forward path. Returns the upstream
    /// response on success, or a [`DispatchError`] on routing /
    /// forwarding failures.
    pub async fn handle<B>(&self, req: Request<B>) -> Result<Response<Incoming>, DispatchError>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        // Hot path: take *borrows* of the request for the route
        // lookup and the upstream lookup, then drop them before
        // moving req into upstream.forward(). The previous
        // implementation cloned host (String), path (String), and
        // the entire HeaderMap before calling find — four heap
        // allocations per request that we can avoid by keeping the
        // borrows alive only as long as the route table needs them.
        //
        // `upstream` is cloned (refcount bump on the Arc-shared
        // Client + Vec<Breaker>) and `total` is `Copy`, so we can
        // hand both off and let the borrows die at end of block.
        let (upstream, total) = {
            let host = req.headers().get(HOST).and_then(|v| v.to_str().ok());
            let path = req.uri().path();
            let headers = req.headers();
            let route = self
                .inner
                .routes
                .find(host, path, headers)
                .ok_or(DispatchError::NoRoute)?;
            let upstream = self
                .inner
                .upstreams
                .get(&route.upstream)
                .cloned()
                .ok_or_else(|| DispatchError::UpstreamNotRegistered {
                    name: route.upstream.clone(),
                })?;
            (upstream, route.total_timeout)
        };

        match tokio::time::timeout(total, upstream.forward(req)).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(DispatchError::Upstream(e)),
            Err(_) => Err(DispatchError::Timeout(total)),
        }
    }
}

/// Errors returned by [`Dispatch::handle`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// No route in the table matched the request's host + path.
    #[error("no route matched the request")]
    NoRoute,

    /// The route resolved to an upstream name that isn't registered.
    /// `conduit-config`'s validation rejects this at load time, so
    /// the only way to trigger it in practice is constructing
    /// `RouteTable` and `UpstreamMap` independently.
    #[error("route references upstream `{name}` which is not registered")]
    UpstreamNotRegistered {
        /// The unresolved upstream name.
        name: String,
    },

    /// The upstream client returned an error.
    #[error("upstream forward failed")]
    Upstream(#[source] ForwardError),

    /// The per-route total timeout fired before the upstream responded.
    /// Surfaces as a `504 Gateway Timeout` to the client.
    #[error("upstream did not respond within {0:?}")]
    Timeout(std::time::Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: Option<&str>, path: PathMatch, upstream: &str) -> Route {
        Route {
            host: host.map(str::to_owned),
            path,
            header_matches: Vec::new(),
            upstream: upstream.to_owned(),
            // Tests build Routes by hand without going through TOML;
            // give them a tame default so timeout-unaware tests don't
            // accidentally fire it.
            total_timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Empty header map for tests that don't exercise header matching.
    fn no_headers() -> http::HeaderMap {
        http::HeaderMap::new()
    }

    fn table(routes: Vec<Route>) -> RouteTable {
        let mut t = RouteTable {
            by_host: HashMap::new(),
            wildcard: HostBucket::default(),
            len: 0,
        };
        for r in routes {
            t.len += 1;
            match r.host.as_deref() {
                Some(h) => {
                    let key = h.to_ascii_lowercase();
                    t.by_host.entry(key).or_default().insert(r);
                }
                None => t.wildcard.insert(r),
            }
        }
        for bucket in t.by_host.values_mut() {
            bucket.finalise();
        }
        t.wildcard.finalise();
        t
    }

    #[test]
    fn first_matching_route_wins() {
        let t = table(vec![
            route(
                Some("api.example.com"),
                PathMatch::Prefix("/v1/".into()),
                "api-v1",
            ),
            route(Some("api.example.com"), PathMatch::Any, "api-default"),
        ]);
        let r = t
            .find(Some("api.example.com"), "/v1/items", &no_headers())
            .unwrap();
        assert_eq!(r.upstream, "api-v1");
        let r = t
            .find(Some("api.example.com"), "/health", &no_headers())
            .unwrap();
        assert_eq!(r.upstream, "api-default");
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let t = table(vec![route(Some("Example.COM"), PathMatch::Any, "u")]);
        assert!(t.find(Some("example.com"), "/", &no_headers()).is_some());
        assert!(t.find(Some("EXAMPLE.COM"), "/", &no_headers()).is_some());
    }

    #[test]
    fn host_none_matches_only_no_host_routes() {
        let t = table(vec![
            route(Some("a.example.com"), PathMatch::Any, "a"),
            route(None, PathMatch::Any, "fallback"),
        ]);
        assert_eq!(
            t.find(None, "/", &no_headers()).unwrap().upstream,
            "fallback"
        );
        assert_eq!(
            t.find(Some("a.example.com"), "/", &no_headers())
                .unwrap()
                .upstream,
            "a"
        );
        assert_eq!(
            t.find(Some("b.example.com"), "/", &no_headers())
                .unwrap()
                .upstream,
            "fallback"
        );
    }

    #[test]
    fn path_exact_does_not_match_prefix() {
        let t = table(vec![route(None, PathMatch::Exact("/healthz".into()), "h")]);
        assert!(t.find(None, "/healthz", &no_headers()).is_some());
        assert!(t.find(None, "/healthz/", &no_headers()).is_none());
        assert!(t.find(None, "/healthzy", &no_headers()).is_none());
    }

    #[test]
    fn path_prefix_match() {
        let t = table(vec![route(None, PathMatch::Prefix("/api/".into()), "u")]);
        assert!(t.find(None, "/api/", &no_headers()).is_some());
        assert!(t.find(None, "/api/users", &no_headers()).is_some());
        assert!(t.find(None, "/api", &no_headers()).is_none()); // no trailing slash
    }

    #[test]
    fn no_match_returns_none() {
        let t = table(vec![route(Some("a.com"), PathMatch::Any, "a")]);
        assert!(t.find(Some("b.com"), "/", &no_headers()).is_none());
        assert!(t.find(None, "/", &no_headers()).is_none());
    }

    #[test]
    fn longest_prefix_wins_within_a_host_bucket() {
        // Declaration order is shorter-first; the trie-equivalent
        // bucket must still pick the longer (more specific) prefix.
        let t = table(vec![
            route(None, PathMatch::Prefix("/api/".into()), "api-root"),
            route(None, PathMatch::Prefix("/api/v2/".into()), "api-v2"),
            route(
                None,
                PathMatch::Prefix("/api/v2/admin/".into()),
                "api-v2-admin",
            ),
        ]);
        assert_eq!(
            t.find(None, "/api/health", &no_headers()).unwrap().upstream,
            "api-root"
        );
        assert_eq!(
            t.find(None, "/api/v2/items", &no_headers())
                .unwrap()
                .upstream,
            "api-v2"
        );
        assert_eq!(
            t.find(None, "/api/v2/admin/users", &no_headers())
                .unwrap()
                .upstream,
            "api-v2-admin"
        );
    }

    #[test]
    fn exact_beats_prefix_within_a_bucket() {
        let t = table(vec![
            route(None, PathMatch::Prefix("/api/".into()), "api"),
            route(None, PathMatch::Exact("/api/health".into()), "health"),
        ]);
        assert_eq!(
            t.find(None, "/api/health", &no_headers()).unwrap().upstream,
            "health",
            "exact match should beat prefix in the same bucket",
        );
        assert_eq!(
            t.find(None, "/api/users", &no_headers()).unwrap().upstream,
            "api"
        );
    }

    #[test]
    fn regex_match_falls_through_after_prefix() {
        // Routes:
        //   /static/   prefix → upstream "static"
        //   ^/v\d+/.*  regex  → upstream "versioned"
        let re = regex::Regex::new(r"^/v\d+/.*").unwrap();
        let t = table(vec![
            route(None, PathMatch::Prefix("/static/".into()), "static"),
            route(None, PathMatch::Regex(re), "versioned"),
        ]);
        assert_eq!(
            t.find(None, "/static/x", &no_headers()).unwrap().upstream,
            "static"
        );
        assert_eq!(
            t.find(None, "/v1/items", &no_headers()).unwrap().upstream,
            "versioned"
        );
        assert_eq!(
            t.find(None, "/v42/foo", &no_headers()).unwrap().upstream,
            "versioned"
        );
        assert!(t.find(None, "/other", &no_headers()).is_none());
    }

    #[test]
    fn regex_first_match_wins_in_declaration_order() {
        let re_a = regex::Regex::new(r"^/api/.+").unwrap();
        let re_b = regex::Regex::new(r"^/api/v2/.+").unwrap();
        // Declaration order: re_a first; even though re_b is more
        // specific, the first regex that matches is returned.
        let t = table(vec![
            route(None, PathMatch::Regex(re_a), "first"),
            route(None, PathMatch::Regex(re_b), "second"),
        ]);
        assert_eq!(
            t.find(None, "/api/v2/items", &no_headers())
                .unwrap()
                .upstream,
            "first"
        );
    }

    #[test]
    fn host_keyed_routes_preferred_over_wildcard() {
        let t = table(vec![
            route(None, PathMatch::Prefix("/".into()), "fallback"),
            route(
                Some("api.example.com"),
                PathMatch::Prefix("/".into()),
                "api",
            ),
        ]);
        // Host-keyed bucket finds a match; wildcard is not consulted.
        assert_eq!(
            t.find(Some("api.example.com"), "/x", &no_headers())
                .unwrap()
                .upstream,
            "api",
        );
        // Unknown host falls back to wildcard.
        assert_eq!(
            t.find(Some("other.com"), "/x", &no_headers())
                .unwrap()
                .upstream,
            "fallback"
        );
    }

    #[test]
    fn upstream_map_round_trip() {
        let mut m = UpstreamMap::new();
        let u = Upstream::new(conduit_upstream::RetryPolicy::default());
        let prev = m.insert("api", u.clone());
        assert!(prev.is_none());
        assert!(m.get("api").is_some());
        assert!(m.get("missing").is_none());
    }
}
